import {
  aggregateByBash,
  aggregateByFile,
  aggregateBySubagent,
  attributeWaste,
  loadPricing,
  type FileAggregation,
} from '@relayburn/analyze';
import { queryAll, readContent, type Query } from '@relayburn/ledger';
import type { ContentRecord } from '@relayburn/reader';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';

const DEFAULT_TOP_N = 10;

export async function runWaste(args: ParsedArgs): Promise<number> {
  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };

  await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll(q);

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

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          turnsAnalyzed: turns.length,
          grandTotal: result.grandTotal,
          attributedTotal: result.attributedTotal,
          unattributedTotal: result.unattributedTotal,
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

  const out: string[] = [];
  out.push('');
  out.push(`turns analyzed: ${formatInt(turns.length)}`);
  out.push(`session grand total: ${formatUsd(result.grandTotal)}`);
  out.push(
    `attributed to tool calls: ${formatUsd(result.attributedTotal)}  /  unattributed (output, system overhead, untracked): ${formatUsd(result.unattributedTotal)}`,
  );
  const evenSplitSessions = result.sessionTotals.filter((s) => s.attributionMethod === 'even-split');
  if (evenSplitSessions.length > 0 && evenSplitSessions.length === result.sessionTotals.length) {
    out.push(
      'note: no content sidecar data found — using even-split (initial cost only). Enable content.store=full in your config to get persistence attribution.',
    );
  } else if (evenSplitSessions.length > 0) {
    out.push(
      `note: ${evenSplitSessions.length}/${result.sessionTotals.length} sessions used even-split (no content sidecar).`,
    );
  }
  out.push('');

  out.push('Top files by cumulative cost');
  if (files.length === 0) {
    out.push('  (no Read/Edit/Write tool calls)');
  } else {
    out.push(renderFileTable(files, limit, result.attributedTotal));
  }
  out.push('');

  out.push('Top Bash commands by cost');
  if (bashes.length === 0) {
    out.push('  (no Bash tool calls)');
  } else {
    out.push(renderBashTable(bashes, limit));
  }
  out.push('');

  out.push('Top subagent calls by cost');
  if (subagents.length === 0) {
    out.push('  (no Agent/Task tool calls)');
  } else {
    out.push(renderSubagentTable(subagents, limit));
  }
  out.push('');

  process.stdout.write(out.join('\n'));
  return 0;
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

function renderBashTable(bashes: ReturnType<typeof aggregateByBash>, limit: number): string {
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

function renderSubagentTable(subagents: ReturnType<typeof aggregateBySubagent>, limit: number): string {
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
