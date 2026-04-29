import {
  aggregateByBash,
  aggregateByFile,
  aggregateBySubagent,
  attributeWaste,
  detectPatterns,
  loadPricing,
  type PatternsResult,
} from '@relayburn/analyze';
import {
  loadConfig,
  queryAll,
  queryCompactions,
  queryRelationships,
  queryToolResultEvents,
  queryUserTurns,
  readContent,
} from '@relayburn/ledger';
import type {
  ContentRecord,
  SessionRelationshipRecord,
  SourceKind,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { countToolCallGaps, ingestAll } from '../ingest.js';
import { formatInt, formatUsd, table } from '../format.js';
import type { ParsedArgs } from '../args.js';

export async function runDiagnose(args: ParsedArgs): Promise<number> {
  const sessionId = args.positional[0];
  if (!sessionId) {
    return runDiagnoseAggregate(args);
  }

  await ingestAll();
  const pricing = await loadPricing();
  const turns = await queryAll({ sessionId });
  if (turns.length === 0) {
    process.stderr.write(`burn diagnose: no turns found for session ${sessionId}\n`);
    return 1;
  }
  const compactions = await queryCompactions({ sessionId });
  const relationships = await queryRelationships({ sessionId });
  const graphSessionIds = collectGraphSessionIds(sessionId, relationships);
  const eventBatches = await Promise.all(
    [...graphSessionIds].map((sid) => queryToolResultEvents({ sessionId: sid })),
  );
  const toolResultEvents = sortToolResultEvents(
    eventBatches.reduce<ToolResultEventRecord[]>((out, batch) => {
      out.push(...batch);
      return out;
    }, []),
  );
  const toolResultStatusBySession = summarizeToolResultStatuses(toolResultEvents);

  const contentRecords: ContentRecord[] = await readContent({ sessionId });
  const contentBySession = new Map<string, ContentRecord[]>();
  if (contentRecords.length > 0) contentBySession.set(sessionId, contentRecords);
  const userTurns: UserTurnRecord[] = await queryUserTurns({ sessionId });
  const userTurnsBySession = new Map<string, UserTurnRecord[]>();
  if (userTurns.length > 0) userTurnsBySession.set(sessionId, userTurns);

  const attribution = attributeWaste(turns, {
    pricing,
    contentBySession,
    userTurnsBySession,
  });
  const patterns = detectPatterns(turns, {
    pricing,
    compactions,
    userTurnsBySession,
    contentBySession,
  });
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
          relationships,
          toolResultEvents,
          toolResultStatusBySession,
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
  if (toolResultStatusBySession.length > 0) {
    out.push(renderToolResultStatusLine(toolResultStatusBySession));
  }
  if (totals) {
    out.push(
      `cost: ${formatUsd(totals.grandCost)} (attributed ${formatUsd(totals.attributedCost)}, unattributed ${formatUsd(totals.unattributedCost)})`,
    );
  }
  if (summary) {
    out.push(
      `patterns: ${summary.retryLoopCount} retry-loops, ${summary.failureRunCount} failure-runs (max ${summary.consecutiveFailureMax}), ${summary.compactionCount} compactions, ${summary.editRevertCount} edit-reverts, ${summary.editHeavyCount} edit-heavy, ${summary.skillRecallDupCount} skill-recall-dups, ${summary.skillPruningProtectionCount} skill-pruning, ${summary.systemPromptTaxCount} system-prompt-tax`,
    );
    out.push(`pattern cost: ${formatUsd(summary.totalPatternCost)}`);
  } else {
    out.push('patterns: none detected');
  }
  out.push('');

  const scoped = filterPatterns(patterns, sessionId);

  if (relationships.length > 0) {
    out.push('Session relationships');
    out.push(renderRelationships(relationships));
    out.push('');
  }
  if (toolResultEvents.length > 0) {
    out.push('Tool result chronology');
    out.push(renderToolResultChronology(toolResultEvents));
    out.push('');
  }

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
  out.push('Edit-heavy session signal');
  out.push(renderEditHeavy(scoped.editHeavySessions));
  out.push('');
  out.push('OpenCode skill recall duplicates');
  out.push(renderSkillRecall(scoped.skillRecallDups));
  out.push('');
  out.push('OpenCode skill pruning protection');
  out.push(renderSkillPruning(scoped.skillPruningProtection));
  out.push('');
  out.push('OpenCode system prompt / skill catalog tax');
  out.push(renderSystemPrompt(scoped.systemPromptTaxes));
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

const DASH = '—';
const RELATIONSHIP_ORDER = new Map<string, number>([
  ['root', 0],
  ['subagent', 1],
  ['continuation', 2],
  ['fork', 3],
]);

interface ToolResultStatusSummary {
  sessionId: string;
  toolCalls: number;
  completed: number;
  errored: number;
  cancelled: number;
  unknown: number;
}

function collectGraphSessionIds(
  sessionId: string,
  relationships: readonly SessionRelationshipRecord[],
): Set<string> {
  const out = new Set<string>([sessionId]);
  for (const r of relationships) {
    out.add(r.sessionId);
    if (r.relatedSessionId) out.add(r.relatedSessionId);
  }
  return out;
}

function sortToolResultEvents(
  events: readonly ToolResultEventRecord[],
): ToolResultEventRecord[] {
  return [...events].sort((a, b) => {
    const session = a.sessionId.localeCompare(b.sessionId);
    if (session !== 0) return session;
    const tool = a.toolUseId.localeCompare(b.toolUseId);
    if (tool !== 0) return tool;
    return a.eventIndex - b.eventIndex;
  });
}

function summarizeToolResultStatuses(
  events: readonly ToolResultEventRecord[],
): ToolResultStatusSummary[] {
  const bySession = new Map<string, Map<string, ToolResultEventRecord[]>>();
  for (const event of events) {
    let byTool = bySession.get(event.sessionId);
    if (!byTool) {
      byTool = new Map();
      bySession.set(event.sessionId, byTool);
    }
    const bucket = byTool.get(event.toolUseId);
    if (bucket) bucket.push(event);
    else byTool.set(event.toolUseId, [event]);
  }

  const out: ToolResultStatusSummary[] = [];
  for (const [sessionId, byTool] of bySession) {
    const row: ToolResultStatusSummary = {
      sessionId,
      toolCalls: 0,
      completed: 0,
      errored: 0,
      cancelled: 0,
      unknown: 0,
    };
    for (const toolEvents of byTool.values()) {
      row.toolCalls++;
      const status = terminalToolStatus(toolEvents);
      row[status]++;
    }
    out.push(row);
  }
  return out.sort((a, b) => a.sessionId.localeCompare(b.sessionId));
}

function terminalToolStatus(
  events: readonly ToolResultEventRecord[],
): 'completed' | 'errored' | 'cancelled' | 'unknown' {
  const sorted = sortToolResultEvents(events);
  const latest = sorted[sorted.length - 1];
  switch (latest?.status) {
    case 'completed':
    case 'errored':
    case 'cancelled':
      return latest.status;
    default:
      return 'unknown';
  }
}

function renderToolResultStatusLine(
  summaries: readonly ToolResultStatusSummary[],
): string {
  const totals = summaries.reduce<ToolResultStatusSummary>(
    (acc, row) => {
      acc.toolCalls += row.toolCalls;
      acc.completed += row.completed;
      acc.errored += row.errored;
      acc.cancelled += row.cancelled;
      acc.unknown += row.unknown;
      return acc;
    },
    {
      sessionId: '',
      toolCalls: 0,
      completed: 0,
      errored: 0,
      cancelled: 0,
      unknown: 0,
    },
  );
  const callWord = totals.toolCalls === 1 ? 'tool call' : 'tool calls';
  const sessionSuffix =
    summaries.length > 1 ? ` across ${formatInt(summaries.length)} linked sessions` : '';
  return `tool results: ${formatInt(totals.errored)} of ${formatInt(totals.toolCalls)} ${callWord} errored${sessionSuffix} (completed ${formatInt(totals.completed)}, cancelled ${formatInt(totals.cancelled)}, unknown ${formatInt(totals.unknown)})`;
}

function renderRelationships(
  relationships: readonly SessionRelationshipRecord[],
): string {
  const sorted = [...relationships].sort((a, b) => {
    const aOrder = RELATIONSHIP_ORDER.get(a.relationshipType) ?? 99;
    const bOrder = RELATIONSHIP_ORDER.get(b.relationshipType) ?? 99;
    if (aOrder !== bOrder) return aOrder - bOrder;
    const session = a.sessionId.localeCompare(b.sessionId);
    if (session !== 0) return session;
    return (a.ts ?? '').localeCompare(b.ts ?? '');
  });
  return table([
    ['session', 'type', 'related', 'source', 'subagentType', 'parentToolUseId', 'description'],
    ...sorted.map((r) => [
      truncate(r.sessionId, 24),
      r.relationshipType,
      truncate(r.relatedSessionId ?? DASH, 24),
      r.source,
      truncate(r.subagentType ?? DASH, 24),
      truncate(r.parentToolUseId ?? DASH, 24),
      truncate(r.description ?? DASH, 36),
    ]),
  ]);
}

function renderToolResultChronology(
  events: readonly ToolResultEventRecord[],
): string {
  const errorCounts = new Map<string, number>();
  for (const event of events) {
    if (event.status !== 'errored') continue;
    const key = toolEventKey(event);
    errorCounts.set(key, (errorCounts.get(key) ?? 0) + 1);
  }
  return table([
    ['toolUseId', 'session', 'event', 'ts', 'status', 'eventSource', 'contentLength'],
    ...sortToolResultEvents(events).map((event) => [
      truncate(event.toolUseId, 24),
      truncate(event.sessionId, 24),
      formatInt(event.eventIndex),
      truncate(event.ts ?? DASH, 24),
      renderToolEventStatus(event, errorCounts.get(toolEventKey(event)) ?? 0),
      event.eventSource,
      event.contentLength === undefined ? DASH : formatInt(event.contentLength),
    ]),
  ]);
}

function toolEventKey(event: ToolResultEventRecord): string {
  return `${event.sessionId}\0${event.toolUseId}`;
}

function renderToolEventStatus(
  event: ToolResultEventRecord,
  erroredEventsForTool: number,
): string {
  if (event.status === 'errored' && erroredEventsForTool > 1) {
    return `errored (${formatInt(erroredEventsForTool)}x)`;
  }
  return event.status;
}

function filterPatterns(patterns: PatternsResult, sessionId: string): PatternsResult {
  return {
    retryLoops: patterns.retryLoops.filter((r) => r.sessionId === sessionId),
    failureRuns: patterns.failureRuns.filter((r) => r.sessionId === sessionId),
    compactions: patterns.compactions.filter((r) => r.sessionId === sessionId),
    editReverts: patterns.editReverts.filter((r) => r.sessionId === sessionId),
    skillRecallDups: patterns.skillRecallDups.filter((r) => r.sessionId === sessionId),
    skillPruningProtection: patterns.skillPruningProtection.filter((r) => r.sessionId === sessionId),
    systemPromptTaxes: patterns.systemPromptTaxes.filter((r) => r.sessionId === sessionId),
    editHeavySessions: patterns.editHeavySessions.filter((r) => r.sessionId === sessionId),
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

function renderSkillRecall(dups: PatternsResult['skillRecallDups']): string {
  if (dups.length === 0) return '  (none)';
  return table([
    ['skill', 'calls', 'turns', 'cost'],
    ...dups.map((d) => [
      truncate(d.skillName, 30),
      String(d.callCount),
      `${d.firstTurnIndex}–${d.lastTurnIndex}`,
      formatUsd(d.cost),
    ]),
  ]);
}

function renderSkillPruning(events: PatternsResult['skillPruningProtection']): string {
  if (events.length === 0) return '  (none)';
  return table([
    ['skill', 'invokedAt', 'ridingTurns', 'lastCached', 'cost'],
    ...events.map((e) => [
      truncate(e.skillName, 30),
      String(e.invokedTurnIndex),
      String(e.ridingTurns),
      String(e.lastCachedTurnIndex),
      formatUsd(e.cost),
    ]),
  ]);
}

function renderEditHeavy(sessions: PatternsResult['editHeavySessions']): string {
  if (sessions.length === 0) return '  (none)';
  return table([
    ['source', 'reads', 'edits', 'ratio', 'retries', 'cost'],
    ...sessions.map((s) => [
      s.source,
      String(s.readCount),
      String(s.editCount),
      Number.isFinite(s.ratio) ? s.ratio.toFixed(1) : '∞',
      String(s.likelyRetries),
      formatUsd(s.cost),
    ]),
  ]);
}

function renderSystemPrompt(taxes: PatternsResult['systemPromptTaxes']): string {
  if (taxes.length === 0) return '  (none)';
  return table([
    ['prefix(tok)', 'userMsg(tok)', 'systemPrompt(tok)', 'ridingTurns', 'cost'],
    ...taxes.map((t) => [
      formatInt(t.firstTurnCacheCreate),
      formatInt(t.firstUserMessageTokens),
      formatInt(t.estimatedSystemPromptTokens),
      formatInt(t.ridingTurns),
      formatUsd(t.totalCost),
    ]),
  ]);
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s;
  return s.slice(0, n - 1) + '…';
}

// Aggregate per-adapter content-capture gap report (#79) plus relationship
// attribution drift (#103). Walks the ledger and tells the user how many
// sessions per adapter have ≥1 tool call but zero `tool_result` ContentRecords
// — the symptom that motivated #58 / #59. Unlike the per-invocation ingest
// warning (which fires once per `burn` run for a fresh affected session), this
// is a permanent, queryable surface.
type Adapter = 'claude' | 'codex' | 'opencode';
const ADAPTER_ORDER: Adapter[] = ['claude', 'codex', 'opencode'];

interface AdapterRow {
  adapter: Adapter;
  sessions: number;
  sessionsWithToolCalls: number;
  // null when contentMode is hash-only / off — we can't tell gap from
  // "store disabled", so we omit the signal rather than mislead.
  gappedSessions: number | null;
  orphanToolCalls: number | null;
  degradedPct: number | null;
}

interface RelationshipDriftDetail {
  sessionId: string;
  adapter: Adapter;
  reason: 'spawn-env-without-native';
  envParentAgentId: string;
}

interface RelationshipDriftReport {
  sessions: number;
  details: RelationshipDriftDetail[];
}

async function runDiagnoseAggregate(args: ParsedArgs): Promise<number> {
  await ingestAll();
  const config = await loadConfig();
  const contentMode = config.content.store;
  const turns = await queryAll();
  const relationships = await queryRelationships();

  const sessionsByAdapter = new Map<Adapter, Map<string, TurnRecord[]>>();
  const adapterBySession = new Map<string, Adapter>();
  for (const t of turns) {
    const adapter = sourceToAdapter(t.source);
    if (!adapter) continue;
    adapterBySession.set(t.sessionId, adapter);
    let bySession = sessionsByAdapter.get(adapter);
    if (!bySession) {
      bySession = new Map();
      sessionsByAdapter.set(adapter, bySession);
    }
    let bucket = bySession.get(t.sessionId);
    if (!bucket) {
      bucket = [];
      bySession.set(t.sessionId, bucket);
    }
    bucket.push(t);
  }

  const rows: AdapterRow[] = [];
  for (const adapter of ADAPTER_ORDER) {
    const bySession = sessionsByAdapter.get(adapter);
    if (!bySession || bySession.size === 0) continue;
    let gapped = 0;
    let orphans = 0;
    let withToolCalls = 0;
    for (const [sid, sessionTurns] of bySession) {
      const hasToolCalls = sessionTurns.some((t) => t.toolCalls.length > 0);
      if (!hasToolCalls) continue;
      withToolCalls++;
      if (contentMode !== 'full') continue;
      const content = await readContent({ sessionId: sid });
      const { sessionAffected, orphanToolCalls } = countToolCallGaps(
        sessionTurns,
        content,
      );
      if (sessionAffected) {
        gapped++;
        orphans += orphanToolCalls;
      }
    }
    const row: AdapterRow = {
      adapter,
      sessions: bySession.size,
      sessionsWithToolCalls: withToolCalls,
      gappedSessions: contentMode === 'full' ? gapped : null,
      orphanToolCalls: contentMode === 'full' ? orphans : null,
      degradedPct:
        contentMode === 'full'
          ? withToolCalls > 0
            ? (gapped / withToolCalls) * 100
            : 0
          : null,
    };
    rows.push(row);
  }
  const relationshipDrift = findRelationshipDrift(relationships, adapterBySession);

  if (args.flags['json'] === true) {
    const payload: {
      adapters: AdapterRow[];
      contentMode: string;
      relationshipDrift: RelationshipDriftReport | { sessions: number };
    } = {
      adapters: rows,
      contentMode,
      relationshipDrift:
        args.flags['explain-drift'] === true
          ? relationshipDrift
          : { sessions: relationshipDrift.sessions },
    };
    process.stdout.write(
      JSON.stringify(payload, null, 2) + '\n',
    );
    return 0;
  }

  const out: string[] = [];
  out.push('');
  out.push('Content-capture gaps by adapter');
  if (rows.length === 0) {
    out.push('  (no sessions in ledger)');
    out.push('');
    process.stdout.write(out.join('\n'));
    return 0;
  }
  if (contentMode !== 'full') {
    out.push(
      `  content store is ${contentMode}; gap signal unavailable. Set RELAYBURN_CONTENT_STORE=full to enable.`,
    );
    out.push(
      table([
        ['adapter', 'sessions', 'withToolCalls'],
        ...rows.map((r) => [
          r.adapter,
          formatInt(r.sessions),
          formatInt(r.sessionsWithToolCalls),
        ]),
      ]),
    );
  } else {
    out.push(
      table([
        [
          'adapter',
          'sessions',
          'withToolCalls',
          'gapped',
          'orphanToolCalls',
          'degraded%',
        ],
        ...rows.map((r) => [
          r.adapter,
          formatInt(r.sessions),
          formatInt(r.sessionsWithToolCalls),
          formatInt(r.gappedSessions ?? 0),
          formatInt(r.orphanToolCalls ?? 0),
          formatPct(r.degradedPct ?? 0),
        ]),
      ]),
    );
  }
  out.push('');
  out.push('Relationship attribution drift');
  out.push(`  sessions: ${formatInt(relationshipDrift.sessions)}`);
  if (args.flags['explain-drift'] === true) {
    if (relationshipDrift.details.length === 0) {
      out.push('  (none)');
    } else {
      out.push(
        table([
          ['session', 'adapter', 'reason', 'envParentAgentId'],
          ...relationshipDrift.details.map((d) => [
            d.sessionId,
            d.adapter,
            d.reason,
            d.envParentAgentId,
          ]),
        ]),
      );
    }
  }
  out.push('');
  process.stdout.write(out.join('\n'));
  return 0;
}

function findRelationshipDrift(
  relationships: readonly SessionRelationshipRecord[],
  adapterBySession: ReadonlyMap<string, Adapter>,
): RelationshipDriftReport {
  const spawnEnvBySession = new Map<string, SessionRelationshipRecord[]>();
  const nativeBySession = new Map<string, SessionRelationshipRecord[]>();
  for (const r of relationships) {
    if (r.relationshipType !== 'subagent') continue;
    if (r.source === 'spawn-env') {
      pushRelationship(spawnEnvBySession, r);
    } else if (r.source === 'native-claude' || r.source === 'native-opencode') {
      pushRelationship(nativeBySession, r);
    }
  }

  const details: RelationshipDriftDetail[] = [];
  for (const [sessionId, envRows] of spawnEnvBySession) {
    const adapter = adapterBySession.get(sessionId);
    if (adapter !== 'claude' && adapter !== 'opencode') continue;
    const nativeRows = nativeBySession.get(sessionId) ?? [];
    if (nativeRows.length > 0) continue;
    const envParentAgentId = firstRelatedSessionId(envRows);
    if (envParentAgentId === undefined) continue;
    details.push({
      sessionId,
      adapter,
      reason: 'spawn-env-without-native',
      envParentAgentId,
    });
  }

  details.sort((a, b) => a.sessionId.localeCompare(b.sessionId));
  return { sessions: details.length, details };
}

function pushRelationship(
  target: Map<string, SessionRelationshipRecord[]>,
  relationship: SessionRelationshipRecord,
): void {
  let list = target.get(relationship.sessionId);
  if (!list) {
    list = [];
    target.set(relationship.sessionId, list);
  }
  list.push(relationship);
}

function firstRelatedSessionId(
  relationships: readonly SessionRelationshipRecord[],
): string | undefined {
  for (const r of relationships) {
    if (typeof r.relatedSessionId === 'string' && r.relatedSessionId.length > 0) {
      return r.relatedSessionId;
    }
  }
  return undefined;
}

function sourceToAdapter(source: SourceKind): Adapter | null {
  switch (source) {
    case 'claude-code':
      return 'claude';
    case 'codex':
      return 'codex';
    case 'opencode':
      return 'opencode';
    default:
      // API-direct sources don't correspond to a parser/harness adapter.
      return null;
  }
}

function formatPct(n: number): string {
  // Match the precision that motivated this report (#58: a 99.7% reading was
  // the original symptom). One decimal place keeps small differences visible.
  return `${n.toFixed(1)}%`;
}
