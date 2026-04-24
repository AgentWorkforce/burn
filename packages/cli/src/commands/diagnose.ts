import {
  aggregateByBash,
  aggregateByFile,
  aggregateBySubagent,
  attributeWaste,
  detectPatterns,
  loadPricing,
  type PatternsResult,
} from '@relayburn/analyze';
import { queryAll, queryCompactions, readContent } from '@relayburn/ledger';
import type { ContentRecord } from '@relayburn/reader';

import { ingestAll } from '../ingest.js';
import { formatInt, formatUsd, table } from '../format.js';
import type { ParsedArgs } from '../args.js';

export async function runDiagnose(args: ParsedArgs): Promise<number> {
  const sessionId = args.positional[0];
  if (!sessionId) {
    process.stderr.write('burn diagnose: missing session id\n');
    return 2;
  }

  await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll({ sessionId });
  if (turns.length === 0) {
    process.stderr.write(`burn diagnose: no turns found for session ${sessionId}\n`);
    return 1;
  }
  const compactions = await queryCompactions({ sessionId });

  const contentRecords: ContentRecord[] = await readContent({ sessionId });
  const contentBySession = new Map<string, ContentRecord[]>();
  if (contentRecords.length > 0) contentBySession.set(sessionId, contentRecords);

  const attribution = attributeWaste(turns, { pricing, contentBySession });
  const patterns = detectPatterns(turns, { pricing, compactions });
  const files = aggregateByFile(attribution.attributions).slice(0, 5);
  const bashes = aggregateByBash(attribution.attributions).slice(0, 5);
  const subagents = aggregateBySubagent(attribution.attributions).slice(0, 5);

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          sessionId,
          turnsAnalyzed: turns.length,
          totals: attribution.sessionTotals[0] ?? null,
          patterns,
          topFiles: files,
          topBashes: bashes,
          topSubagents: subagents,
        },
        null,
        2,
      ) + '\n',
    );
    return 0;
  }

  const totals = attribution.sessionTotals[0];
  const summary = patterns.sessionSummaries.find((s) => s.sessionId === sessionId);
  const out: string[] = [];
  out.push('');
  out.push(`session: ${sessionId}`);
  out.push(`turns: ${formatInt(turns.length)}`);
  if (totals) {
    out.push(
      `cost: ${formatUsd(totals.grandCost)} (attributed ${formatUsd(totals.attributedCost)}, unattributed ${formatUsd(totals.unattributedCost)})`,
    );
  }
  if (summary) {
    out.push(
      `patterns: ${summary.retryLoopCount} retry-loops, ${summary.failureRunCount} failure-runs (max ${summary.consecutiveFailureMax}), ${summary.compactionCount} compactions, ${summary.editRevertCount} edit-reverts`,
    );
    out.push(`pattern cost: ${formatUsd(summary.totalPatternCost)}`);
  } else {
    out.push('patterns: none detected');
  }
  out.push('');

  const scoped = filterPatterns(patterns, sessionId);

  out.push('Retry loops');
  out.push(renderRetries(scoped.retryLoops));
  out.push('');
  out.push('Consecutive failure runs');
  out.push(renderFailures(scoped.failureRuns));
  out.push('');
  out.push('Compaction events');
  out.push(renderCompactions(scoped.compactions));
  out.push('');
  out.push('Edit-revert cycles');
  out.push(renderReverts(scoped.editReverts));
  out.push('');

  out.push('Top files by cost');
  if (files.length === 0) out.push('  (none)');
  else {
    out.push(
      table([
        ['path', 'calls', 'cost'],
        ...files.map((f) => [truncate(f.path, 50), String(f.toolCallCount), formatUsd(f.totalCost)]),
      ]),
    );
  }
  out.push('');

  out.push('Top Bash commands by cost');
  if (bashes.length === 0) out.push('  (none)');
  else {
    out.push(
      table([
        ['command', 'calls', 'cost'],
        ...bashes.map((b) => [
          truncate(b.command ?? `(hash ${b.argsHash.slice(0, 8)})`, 50),
          String(b.callCount),
          formatUsd(b.totalCost),
        ]),
      ]),
    );
  }
  out.push('');

  out.push('Top subagent calls by cost');
  if (subagents.length === 0) out.push('  (none)');
  else {
    out.push(
      table([
        ['subagent', 'calls', 'cost'],
        ...subagents.map((s) => [s.subagentType, String(s.callCount), formatUsd(s.totalCost)]),
      ]),
    );
  }
  out.push('');

  process.stdout.write(out.join('\n'));
  return 0;
}

function filterPatterns(patterns: PatternsResult, sessionId: string): PatternsResult {
  return {
    retryLoops: patterns.retryLoops.filter((r) => r.sessionId === sessionId),
    failureRuns: patterns.failureRuns.filter((r) => r.sessionId === sessionId),
    compactions: patterns.compactions.filter((r) => r.sessionId === sessionId),
    editReverts: patterns.editReverts.filter((r) => r.sessionId === sessionId),
    sessionSummaries: patterns.sessionSummaries.filter((r) => r.sessionId === sessionId),
  };
}

function renderRetries(loops: PatternsResult['retryLoops']): string {
  if (loops.length === 0) return '  (none)';
  return table([
    ['tool', 'target', 'attempts', 'turns', 'cost'],
    ...loops.map((r) => [
      r.tool,
      truncate(r.target ?? '—', 40),
      String(r.attempts),
      `${r.startTurnIndex}–${r.endTurnIndex}`,
      formatUsd(r.cost),
    ]),
  ]);
}

function renderFailures(runs: PatternsResult['failureRuns']): string {
  if (runs.length === 0) return '  (none)';
  return table([
    ['length', 'turns', 'tools', 'cost'],
    ...runs.map((r) => [
      String(r.length),
      `${r.startTurnIndex}–${r.endTurnIndex}`,
      truncate(r.toolsInvolved.join(', '), 40),
      formatUsd(r.cost),
    ]),
  ]);
}

function renderCompactions(events: PatternsResult['compactions']): string {
  if (events.length === 0) return '  (none)';
  return table([
    ['ts', 'cacheLost(tok)', 'cost'],
    ...events.map((e) => [
      e.ts,
      formatInt(e.tokensBeforeCompact),
      formatUsd(e.cacheLostCost),
    ]),
  ]);
}

function renderReverts(cycles: PatternsResult['editReverts']): string {
  if (cycles.length === 0) return '  (none)';
  return table([
    ['file', 'firstEdit', 'revert', 'span', 'cost'],
    ...cycles.map((c) => [
      truncate(c.filePath, 40),
      String(c.firstEditTurnIndex),
      String(c.revertTurnIndex),
      String(c.spanTurns),
      formatUsd(c.cost),
    ]),
  ]);
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s;
  return s.slice(0, n - 1) + '…';
}
