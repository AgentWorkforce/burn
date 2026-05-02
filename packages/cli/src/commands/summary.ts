import {
  aggregateByProvider,
  aggregateSubagentTypeStats,
  buildSubagentTree,
  computeQuality,
  loadPricing,
  summarizeFidelity,
} from '@relayburn/analyze';
import { costForTurn, sumCosts } from '@relayburn/analyze';
import type {
  CoverageField,
  FieldCoverage,
  FidelitySummary,
  OutcomeLabel,
  QualityResult,
  RowCoverage,
  SubagentTreeNode,
  UsageCostAggregateRow,
} from '@relayburn/analyze';
import {
  buildArchive,
  queryAll,
  queryAllFromArchive,
  queryRelationships,
  queryUserTurns,
  readContent,
  type Query,
} from '@relayburn/ledger';
import type { EnrichedTurn } from '@relayburn/ledger';
import type {
  ContentRecord,
  Coverage,
  RelationshipType,
  SessionRelationshipRecord,
  UserTurnRecord,
} from '@relayburn/reader';

import { ingestAll } from '@relayburn/ingest';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { withProgress } from '../progress.js';
import {
  filterTurnsByProvider,
  parseProviderFilter,
} from '../provider.js';
import type { ProviderFilter } from '../provider.js';

export async function runSummary(args: ParsedArgs): Promise<number> {
  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];
  if (typeof args.flags['workflow'] === 'string') q.enrichment = { workflowId: args.flags['workflow'] };
  const agentFilter = typeof args.flags['agent'] === 'string' ? args.flags['agent'] : undefined;
  const providerFilter = parseProviderFilter(args.flags['provider']);
  if (providerFilter instanceof Error) {
    process.stderr.write(providerFilter.message);
    return 2;
  }

  const subagentTreeFlag = args.flags['subagent-tree'];
  const subagentTypeFlag = args.flags['by-subagent-type'] === true;
  const relationshipFlag = args.flags['by-relationship'];
  const byRelationship = relationshipFlag !== undefined;
  const byProvider = args.flags['by-provider'] === true;
  const byTool = args.flags['by-tool'] === true;
  if (
    relationshipFlag !== undefined &&
    relationshipFlag !== true &&
    relationshipFlag !== 'subagent'
  ) {
    process.stderr.write('burn: --by-relationship accepts only the optional value "subagent"\n');
    return 2;
  }

  // Mode exclusivity: each "mode flag" produces its own output shape; combining
  // them silently would surprise the caller (we'd pick one and drop the rest).
  // Subagent flags already implicitly assume one-axis-at-a-time; --by-tool
  // makes that explicit.
  if (
    byTool &&
    (byProvider || subagentTypeFlag || byRelationship || subagentTreeFlag !== undefined)
  ) {
    process.stderr.write(
      'burn: --by-tool cannot be combined with --by-provider/--by-subagent-type/--by-relationship/--subagent-tree\n',
    );
    return 2;
  }
  if (byProvider && (subagentTypeFlag || byRelationship || subagentTreeFlag !== undefined)) {
    process.stderr.write(
      'burn: --by-provider cannot be combined with --by-subagent-type/--by-relationship/--subagent-tree\n',
    );
    return 2;
  }
  if (subagentTypeFlag && (byRelationship || subagentTreeFlag !== undefined)) {
    process.stderr.write(
      'burn: --by-subagent-type cannot be combined with --by-relationship/--subagent-tree\n',
    );
    return 2;
  }
  if (byRelationship && subagentTreeFlag !== undefined) {
    process.stderr.write(
      'burn: --by-relationship cannot be combined with --subagent-tree\n',
    );
    return 2;
  }

  const ingestReport = await withProgress('ingesting latest sessions', (task) =>
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
  const agentSessionIds =
    agentFilter !== undefined
      ? await withProgress('resolving agent session tree', async (task) => {
          const ids = await resolveAgentSessionTree(agentFilter);
          task.succeed(
            `resolved ${formatInt(ids.size)} linked session${ids.size === 1 ? '' : 's'}`,
          );
          return ids;
        })
      : undefined;
  if (subagentTreeFlag !== undefined) {
    return renderSubagentTreeMode(
      args,
      pricing,
      subagentTreeFlag,
      q,
      agentFilter,
      agentSessionIds,
      providerFilter,
    );
  }

  const turns = filterTurnsByProvider(
    filterTurnsByAgent(await loadTurns(q, args), agentFilter, agentSessionIds),
    providerFilter,
  );

  if (subagentTypeFlag) {
    return renderSubagentTypeMode(args, turns, pricing);
  }
  if (byRelationship) {
    return renderRelationshipMode(args, turns, pricing, q, relationshipFlag);
  }
  if (byTool) {
    return renderByToolMode(args, ingestReport, turns, pricing);
  }

  const rows = byProvider
    ? aggregateByProvider(turns, { pricing })
    : aggregateByModel(turns, pricing);
  const totalCost = sumCosts(rows.map((r) => r.cost));
  const fidelity = summarizeFidelity(turns);

  if (args.flags['json'] === true) {
    // JSON contract: numeric usage fields are always numbers, but the
    // companion `fidelity` block is the only honest answer to "are these
    // zeros real?". Programmatic consumers should consult `summary` (the
    // slice-wide rollup, same shape compare/hotspots emit) and
    // `perCell` (per-(model|provider) per-field known/missing counts) before
    // trusting any aggregate.
    const perCell = buildPerCellFidelity(rows, byProvider ? 'provider' : 'model');
    const payload = {
      ingest: {
        ingestedSessions: ingestReport.ingestedSessions,
        appendedTurns: ingestReport.appendedTurns,
      },
      turns: turns.length,
      totalCost,
      ...(byProvider
        ? {
            byProvider: rows.map((r) => ({
              provider: r.label,
              turns: r.turns,
              usage: r.usage,
              cost: r.cost,
            })),
          }
        : {
            byModel: rows.map((r) => ({
              model: r.label,
              turns: r.turns,
              usage: r.usage,
              cost: r.cost,
            })),
          }),
      fidelity: { summary: fidelity, perCell },
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

  const header = [byProvider ? 'provider' : 'model', 'turns', 'input', 'output', 'reasoning', 'cacheRead', 'cacheCreate', 'cost'];
  const dataRows: string[][] = [header];
  let anyPartialCell = false;
  for (const r of rows) {
    const cacheCreateCov = mergeCoverage(r.coverage.cacheCreate);
    if (
      cellIsPartial(r.coverage.input) ||
      cellIsPartial(r.coverage.output) ||
      cellIsPartial(r.coverage.reasoning) ||
      cellIsPartial(r.coverage.cacheRead) ||
      cellIsPartial(cacheCreateCov)
    ) {
      anyPartialCell = true;
    }
    dataRows.push([
      r.label,
      formatInt(r.turns),
      coverageCell(r.usage.input, r.coverage.input),
      coverageCell(r.usage.output, r.coverage.output),
      coverageCell(r.usage.reasoning, r.coverage.reasoning),
      coverageCell(r.usage.cacheRead, r.coverage.cacheRead),
      coverageCell(r.usage.cacheCreate5m + r.usage.cacheCreate1h, cacheCreateCov),
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

  // Footer marker explainer: only print when at least one cell carries `*`
  // — the all-full case is the common one and we don't want to train people
  // to ignore the marker.
  if (anyPartialCell) {
    lines.push(formatPartialFooter(rows));
    lines.push('');
  }

  // Only print a fidelity line when *something* is below full — the common
  // all-Claude case is full fidelity for every turn, and noise there would
  // train people to ignore the line in cases that actually matter.
  const fidelityNotice = renderFidelityNotice(fidelity);
  if (fidelityNotice) {
    lines.push(fidelityNotice);
    lines.push('');
  }

  if (args.flags['quality'] === true) {
    const contentBySession = await withProgress('reading content for quality summary', async (task) => {
      const content = await loadContentForQuality(turns);
      task.succeed(`read content for ${formatInt(content.size)} session${content.size === 1 ? '' : 's'}`);
      return content;
    });
    const quality = await withProgress('computing quality summary', async (task) => {
      const result = computeQuality(turns, { contentBySession });
      task.succeed('computed quality summary');
      return result;
    });
    lines.push(renderQuality(quality));
    lines.push('');
  }

  process.stdout.write(lines.join('\n'));
  return 0;
}

async function resolveAgentSessionTree(agentId: string): Promise<Set<string>> {
  return collectAgentSessionTree(await queryRelationships(), agentId);
}

function collectAgentSessionTree(
  relationships: readonly SessionRelationshipRecord[],
  agentId: string,
): Set<string> {
  const byParent = new Map<string, SessionRelationshipRecord[]>();
  for (const r of relationships) {
    if (r.relationshipType !== 'subagent') continue;
    if (typeof r.relatedSessionId !== 'string' || r.relatedSessionId.length === 0) continue;
    let list = byParent.get(r.relatedSessionId);
    if (!list) {
      list = [];
      byParent.set(r.relatedSessionId, list);
    }
    list.push(r);
  }

  const sessions = new Set<string>();
  const seenParents = new Set<string>();
  const queue = [agentId];
  while (queue.length > 0) {
    const parent = queue.shift()!;
    if (seenParents.has(parent)) continue;
    seenParents.add(parent);
    for (const child of byParent.get(parent) ?? []) {
      sessions.add(child.sessionId);
      queue.push(child.sessionId);
      if (typeof child.agentId === 'string' && child.agentId.length > 0) {
        queue.push(child.agentId);
      }
    }
  }
  return sessions;
}

function filterTurnsByAgent(
  turns: EnrichedTurn[],
  agentId: string | undefined,
  sessionIds: Set<string> | undefined,
): EnrichedTurn[] {
  if (agentId === undefined) return turns;
  return turns.filter((t) => {
    if (t.enrichment['agentId'] === agentId) return true;
    if (t.enrichment['parentAgentId'] === agentId) return true;
    return sessionIds?.has(t.sessionId) === true;
  });
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

// Per-token-field coverage counters maintained alongside each aggregate row.
// `known` is the number of contributing turns whose source actually reported
// the field; `missing` counts turns whose source omitted it (the matching
// `Coverage` flag was false). Records emitted before fidelity metadata
// existed (no `fidelity` at all) are treated as best-effort full and counted
// as `known` — the same backward-compat stance taken by `summarizeFidelity`
// / `hasMinimumFidelity`.
const COVERAGE_FIELDS: ReadonlyArray<CoverageField> = [
  'input',
  'output',
  'reasoning',
  'cacheRead',
  'cacheCreate',
];

const COVERAGE_FLAG: Record<CoverageField, keyof Coverage> = {
  input: 'hasInputTokens',
  output: 'hasOutputTokens',
  reasoning: 'hasReasoningTokens',
  cacheRead: 'hasCacheReadTokens',
  cacheCreate: 'hasCacheCreateTokens',
};

type ModelRow = UsageCostAggregateRow;

async function renderSubagentTreeMode(
  args: ParsedArgs,
  pricing: Parameters<typeof costForTurn>[1],
  flag: string | true,
  q: Query,
  agentFilter: string | undefined,
  agentSessionIds: Set<string> | undefined,
  providerFilter: ProviderFilter | undefined,
): Promise<number> {
  // Accept either `--subagent-tree <id>` or `--subagent-tree` with --session.
  const sessionId = typeof flag === 'string' ? flag : q.sessionId;
  if (!sessionId) {
    process.stderr.write('burn: --subagent-tree requires a session id (positional or --session)\n');
    return 2;
  }
  const relationships = await withProgress('reading subagent relationships', async (task) => {
    const rows = await collectSubagentTreeRelationships(sessionId, q);
    task.succeed(`read ${formatInt(rows.length)} relationship${rows.length === 1 ? '' : 's'}`);
    return rows;
  });
  const turns = filterTurnsByProvider(
    filterTurnsByAgent(
      await withProgress('reading subagent tree turns', async (task) => {
        const rows = await loadSubagentTreeTurns(sessionId, relationships, q, args);
        task.succeed(`read ${formatInt(rows.length)} turn${rows.length === 1 ? '' : 's'}`);
        return rows;
      }),
      agentFilter,
      agentSessionIds,
    ),
    providerFilter,
  );
  const trees = await withProgress('building subagent tree', async (task) => {
    const built = buildSubagentTree(turns, { pricing, relationships });
    task.succeed(`built ${formatInt(built.size)} subagent tree${built.size === 1 ? '' : 's'}`);
    return built;
  });
  const root = trees.get(sessionId) ?? findTreeNode(trees, sessionId);
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

async function collectSubagentTreeRelationships(
  sessionId: string,
  q: Query,
): Promise<SessionRelationshipRecord[]> {
  const queryBase = relationshipQueryForTurnSlice(q);
  const out = new Map<string, SessionRelationshipRecord>();
  const seenIds = new Set<string>();
  const queue = [sessionId];

  while (queue.length > 0) {
    const id = queue.shift()!;
    if (seenIds.has(id)) continue;
    seenIds.add(id);
    const relationships = await queryRelationships({ ...queryBase, sessionId: id });
    for (const r of relationships) {
      out.set(relationshipInstanceKey(r), r);
      for (const next of relationshipConnectedIds(r)) {
        if (next.length > 0 && !seenIds.has(next)) queue.push(next);
      }
    }
  }

  return [...out.values()];
}

function relationshipConnectedIds(r: SessionRelationshipRecord): string[] {
  const ids = [r.sessionId];
  if (r.relatedSessionId !== undefined) ids.push(r.relatedSessionId);
  if (r.agentId !== undefined) ids.push(r.agentId);
  return ids;
}

async function loadSubagentTreeTurns(
  sessionId: string,
  relationships: readonly SessionRelationshipRecord[],
  q: Query,
  args: ParsedArgs,
): Promise<EnrichedTurn[]> {
  const sessionIds = new Set<string>([sessionId]);
  for (const r of relationships) {
    sessionIds.add(r.sessionId);
  }

  const byKey = new Map<string, EnrichedTurn>();
  for (const id of sessionIds) {
    const turns = await loadTurns({ ...q, sessionId: id }, args);
    for (const t of turns) {
      byKey.set(`${t.source}|${t.sessionId}|${t.messageId}`, t);
    }
  }
  return [...byKey.values()];
}

async function renderByToolMode(
  args: ParsedArgs,
  ingestReport: { ingestedSessions: number; appendedTurns: number },
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
): Promise<number> {
  const userTurnsBySession = await withProgress('reading user turns for tool attribution', async (task) => {
    const rows = await loadUserTurnsForByTool(turns);
    task.succeed(`read user turns for ${formatInt(rows.size)} session${rows.size === 1 ? '' : 's'}`);
    return rows;
  });
  const { byTool, unattributed } = await withProgress('attributing tool cost', async (task) => {
    const result = attributeCostToTools(
      turns,
      pricing,
      userTurnsBySession,
    );
    task.succeed(`attributed cost to ${formatInt(result.byTool.size)} tool${result.byTool.size === 1 ? '' : 's'}`);
    return result;
  });
  const fidelity = summarizeFidelity(turns);
  const sorted = [...byTool.entries()].sort((a, b) => b[1].cost - a[1].cost);

  if (args.flags['json'] === true) {
    const payload = {
      ingest: {
        ingestedSessions: ingestReport.ingestedSessions,
        appendedTurns: ingestReport.appendedTurns,
      },
      turns: turns.length,
      byTool: sorted.map(([tool, agg]) => ({
        tool,
        calls: agg.calls,
        attributedCost: agg.cost,
        attributionMethod: toolAttributionMethod(agg),
      })),
      unattributed,
      fidelity: { summary: fidelity },
    };
    process.stdout.write(JSON.stringify(payload, null, 2) + '\n');
    return 0;
  }

  const out: string[] = [];
  out.push('');
  out.push(`turns analyzed: ${formatInt(turns.length)}`);
  out.push('');
  if (sorted.length === 0) {
    out.push('no tool calls found for filters.');
    process.stdout.write(out.join('\n') + '\n');
    return 0;
  }
  const rows: string[][] = [['tool', 'calls', 'attributedCost']];
  for (const [tool, { calls, cost }] of sorted) {
    rows.push([tool, formatInt(calls), formatUsd(cost)]);
  }
  out.push(table(rows));
  out.push('');
  // Keep the explanation of `attributedCost` riding along with the table —
  // mixing the by-tool number (input-cost split across prior-turn tool calls)
  // and the by-model `cost` number without context is an easy way to draw the
  // wrong conclusion.
  out.push(
    `attributedCost = turn N ingest cost assigned to turn N-1 tool_use blocks by user-turn byte size when available, otherwise split evenly.`,
  );
  out.push(
    `unattributed cost (no prior tool call or non-tool user text): ${formatUsd(unattributed)}`,
  );
  out.push('');
  process.stdout.write(out.join('\n'));
  return 0;
}

interface ToolAgg {
  calls: number;
  cost: number;
  sizedCost: number;
  evenSplitCost: number;
}

type ByToolAttributionMethod = 'sized' | 'even-split' | 'unattributed';

interface UserTurnSizeBucket {
  toolBytesById: Map<string, number>;
  totalBytes: number;
}

function attributeCostToTools(
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
  userTurnsBySession: Map<string, UserTurnRecord[]> = new Map(),
): { byTool: Map<string, ToolAgg>; unattributed: number } {
  const byTool = new Map<string, ToolAgg>();
  let unattributed = 0;

  // Group turns by sessionId then pair each turn with the previous turn's toolCalls.
  const bySession = new Map<string, EnrichedTurn[]>();
  for (const t of turns) {
    let list = bySession.get(t.sessionId);
    if (!list) {
      list = [];
      bySession.set(t.sessionId, list);
    }
    list.push(t);
  }

  for (const list of bySession.values()) {
    list.sort((a, b) => a.turnIndex - b.turnIndex);
    const sessionId = list[0]?.sessionId;
    const userTurnSizeIndex =
      sessionId === undefined
        ? new Map<string, UserTurnSizeBucket>()
        : indexUserTurnBlockSizes(userTurnsBySession.get(sessionId) ?? []);
    for (let i = 0; i < list.length; i++) {
      const turn = list[i]!;
      const c = costForTurn(turn, pricing);
      if (!c) continue;
      // cost this turn paid to ingest the PRIOR turn's tool outputs:
      const ingestCost = c.input + c.cacheRead + c.cacheCreate;

      // Also count the tool-calls this turn emits (so they appear even if the next turn is unpriced).
      for (const tc of turn.toolCalls) {
        const agg = byTool.get(tc.name) ?? emptyToolAgg();
        agg.calls++;
        byTool.set(tc.name, agg);
      }

      if (i === 0) {
        unattributed += ingestCost;
        continue;
      }
      const prior = list[i - 1]!;
      if (prior.toolCalls.length === 0) {
        unattributed += ingestCost;
        continue;
      }
      const sizes = userTurnSizeIndex.get(bridgeKey(prior.messageId, turn.messageId));
      const sizedBytes = sizes
        ? prior.toolCalls.reduce(
            (sum, tc) => sum + (sizes.toolBytesById.get(tc.id) ?? 0),
            0,
          )
        : 0;
      if (sizes && sizedBytes > 0) {
        const allocatableCost =
          sizes.totalBytes > 0
            ? ingestCost * Math.min(1, sizedBytes / sizes.totalBytes)
            : ingestCost;
        unattributed += ingestCost - allocatableCost;
        const rawShares: Array<{ tool: string; cost: number }> = [];
        for (const tc of prior.toolCalls) {
          const bytes = sizes.toolBytesById.get(tc.id) ?? 0;
          if (bytes <= 0) continue;
          rawShares.push({ tool: tc.name, cost: (bytes / sizedBytes) * allocatableCost });
        }
        const rawSubtotal = rawShares.reduce((sum, row) => sum + row.cost, 0);
        const scale = rawSubtotal > allocatableCost && rawSubtotal > 0
          ? allocatableCost / rawSubtotal
          : 1;
        for (const row of rawShares) {
          const share = row.cost * scale;
          const agg = byTool.get(row.tool) ?? emptyToolAgg();
          agg.cost += share;
          agg.sizedCost += share;
          byTool.set(row.tool, agg);
        }
      } else {
        const share = ingestCost / prior.toolCalls.length;
        for (const tc of prior.toolCalls) {
          const agg = byTool.get(tc.name) ?? emptyToolAgg();
          agg.cost += share;
          agg.evenSplitCost += share;
          byTool.set(tc.name, agg);
        }
      }
    }
  }

  return { byTool, unattributed };
}

async function loadUserTurnsForByTool(
  turns: EnrichedTurn[],
): Promise<Map<string, UserTurnRecord[]>> {
  const sessionIds = [...new Set(turns.map((t) => t.sessionId))];
  const out = new Map<string, UserTurnRecord[]>();
  for (const sessionId of sessionIds) {
    const userTurns = await queryUserTurns({ sessionId });
    if (userTurns.length > 0) out.set(sessionId, userTurns);
  }
  return out;
}

function indexUserTurnBlockSizes(
  userTurns: readonly UserTurnRecord[],
): Map<string, UserTurnSizeBucket> {
  const out = new Map<string, UserTurnSizeBucket>();
  for (const userTurn of userTurns) {
    if (!userTurn.precedingMessageId || !userTurn.followingMessageId) continue;
    const key = bridgeKey(userTurn.precedingMessageId, userTurn.followingMessageId);
    let bucket = out.get(key);
    if (!bucket) {
      bucket = { toolBytesById: new Map<string, number>(), totalBytes: 0 };
      out.set(key, bucket);
    }
    for (const block of userTurn.blocks) {
      const bytes = Math.max(0, block.byteLen);
      bucket.totalBytes += bytes;
      if (block.kind !== 'tool_result' || !block.toolUseId) continue;
      bucket.toolBytesById.set(
        block.toolUseId,
        (bucket.toolBytesById.get(block.toolUseId) ?? 0) + bytes,
      );
    }
  }
  return out;
}

function bridgeKey(precedingMessageId: string, followingMessageId: string): string {
  return `${precedingMessageId}\0${followingMessageId}`;
}

function emptyToolAgg(): ToolAgg {
  return { calls: 0, cost: 0, sizedCost: 0, evenSplitCost: 0 };
}

function toolAttributionMethod(agg: ToolAgg): ByToolAttributionMethod {
  if (agg.sizedCost === 0 && agg.evenSplitCost === 0) return 'unattributed';
  if (agg.sizedCost >= agg.evenSplitCost) return 'sized';
  return 'even-split';
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

async function renderRelationshipMode(
  args: ParsedArgs,
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
  q: Query,
  flag: string | true,
): Promise<number> {
  const relationships = await withProgress('reading session relationships', async (task) => {
    const rows = await queryRelationships(relationshipQueryForTurnSlice(q));
    task.succeed(`read ${formatInt(rows.length)} relationship${rows.length === 1 ? '' : 's'}`);
    return rows;
  });
  const matches = matchRelationshipsToTurns(relationships, turns, pricing);
  const stats = aggregateRelationshipStats(matches);

  if (flag === 'subagent') {
    return renderRelationshipSubagentMode(args, stats, matches);
  }

  if (stats.length === 0) {
    return renderNoRelationships(args);
  }

  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify({ relationships: stats }, null, 2) + '\n');
    return 0;
  }

  const out: string[] = [];
  out.push('');
  out.push(`relationships: ${formatInt(stats.reduce((sum, s) => sum + s.sessionCount, 0))}`);
  out.push('');
  const rows: string[][] = [
    ['relationshipType', 'sessionCount', 'turnCount', 'total', 'median', 'p95', 'mean'],
  ];
  for (const s of stats) {
    rows.push([
      s.relationshipType,
      formatInt(s.sessionCount),
      formatInt(s.turnCount),
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

function renderRelationshipSubagentMode(
  args: ParsedArgs,
  stats: RelationshipStats[],
  matches: RelationshipMatch[],
): number {
  const subagentStats = aggregateRelationshipSubagentStats(matches);
  if (subagentStats.length === 0) {
    return renderNoRelationships(args);
  }

  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          relationships: stats.filter((s) => s.relationshipType === 'subagent'),
          subagentTypes: subagentStats,
        },
        null,
        2,
      ) + '\n',
    );
    return 0;
  }

  const out: string[] = [];
  out.push('');
  out.push(
    `subagent invocations: ${formatInt(subagentStats.reduce((a, s) => a + s.invocations, 0))}`,
  );
  out.push('');
  const rows: string[][] = [
    ['subagentType', 'invocations', 'turns', 'total', 'median', 'p95', 'mean'],
  ];
  for (const s of subagentStats) {
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

function findTreeNode(
  trees: ReadonlyMap<string, SubagentTreeNode>,
  nodeId: string,
): SubagentTreeNode | undefined {
  for (const root of trees.values()) {
    const found = findNode(root, nodeId);
    if (found) return found;
  }
  return undefined;
}

function findNode(node: SubagentTreeNode, nodeId: string): SubagentTreeNode | undefined {
  if (node.nodeId === nodeId) return node;
  for (const child of node.children) {
    const found = findNode(child, nodeId);
    if (found) return found;
  }
  return undefined;
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
  const relationship =
    node.relationshipType !== 'root' && node.relationshipType !== 'subagent'
      ? ` [${node.relationshipType}]`
      : '';
  const model = node.models.length > 0 ? ` (${node.models.join(', ')})` : '';
  const cost = formatUsd(node.cumulativeCost);
  const turns = `[${formatInt(node.cumulativeTurns)} turn${node.cumulativeTurns === 1 ? '' : 's'}]`;
  return `${indent}${label}${relationship}${model}  ${cost}  ${turns}`;
}

// Render one token-field cell. Three cases:
//   - every contributing turn reported the field → numeric value, no marker
//   - some turns reported, some didn't           → numeric value + `*`
//   - no turn reported                           → `—` (never `0`, which
//     would falsely claim a real zero from the source)
// `cacheCreate` callers pre-merge the 5m/1h split via `mergeCoverage`; they
// share a single coverage flag (`hasCacheCreateTokens`).
function coverageCell(value: number, c: FieldCoverage): string {
  if (c.known === 0 && c.missing > 0) return DASH;
  if (c.known > 0 && c.missing > 0) return `${formatInt(value)}${PARTIAL_MARK}`;
  return formatInt(value);
}

function cellIsPartial(c: FieldCoverage): boolean {
  return c.known > 0 && c.missing > 0;
}

// `cacheCreate5m` and `cacheCreate1h` collapse into one ledger column and one
// `Coverage` flag (`hasCacheCreateTokens`), so the per-cell counter is shared
// between them. This helper just returns that single counter — the second
// argument is intentional dead-code symmetry to make the call sites obvious.
function mergeCoverage(cacheCreate: FieldCoverage): FieldCoverage {
  return cacheCreate;
}

// Footer note explaining the `*` marker. Denominator is the cross-row sum of
// `known + missing` for input — input is the canonical token field, so if a
// record has any per-turn coverage at all, it carries input. Numerator is the
// worst-covered axis: for each coverage field, sum its `missing` across every
// row, then take the max. Picking just `input.missing` would understate the
// gap when the partial coverage is on a *different* field (e.g. an
// output-omitting collector); picking the per-(row, field) max would
// understate it across multi-row aggregates. The cross-row sum per field is
// the right "N of M" — it's the count of turns missing the most-affected
// field across the whole slice.
function formatPartialFooter(rows: ReadonlyArray<ModelRow>): string {
  let total = 0;
  for (const r of rows) {
    total += r.coverage.input.known + r.coverage.input.missing;
  }
  let missing = 0;
  for (const f of COVERAGE_FIELDS) {
    let fieldMissing = 0;
    for (const r of rows) fieldMissing += r.coverage[f].missing;
    if (fieldMissing > missing) missing = fieldMissing;
  }
  return `${PARTIAL_MARK} partial coverage: ${formatInt(missing)} of ${formatInt(total)} turns omitted per-turn token data`;
}

interface PerCellFidelityEntry {
  label: string;
  partial: boolean;
  fields: Record<CoverageField, FieldCoverage>;
}

interface PerCellFidelityBlock {
  groupBy: 'model' | 'provider';
  cells: PerCellFidelityEntry[];
}

// Per-(model|provider) per-field coverage block for `--json`. Shape mirrors
// the pattern compare/hotspots use: a `summary` (slice-wide rollup) plus an
// optional `perCell` payload programmatic callers can render without
// re-walking the ledger. Empty `cells` array (no rows in scope) is a valid
// payload — callers should treat it the same as "every cell is full".
function buildPerCellFidelity(
  rows: ReadonlyArray<ModelRow>,
  groupBy: 'model' | 'provider',
): PerCellFidelityBlock {
  const cells: PerCellFidelityEntry[] = rows.map((r) => {
    const cacheCreate = mergeCoverage(r.coverage.cacheCreate);
    const fields: Record<CoverageField, FieldCoverage> = {
      input: r.coverage.input,
      output: r.coverage.output,
      reasoning: r.coverage.reasoning,
      cacheRead: r.coverage.cacheRead,
      cacheCreate,
    };
    const partial = COVERAGE_FIELDS.some((f) => cellIsPartial(fields[f])) ||
      COVERAGE_FIELDS.some((f) => fields[f].known === 0 && fields[f].missing > 0);
    return { label: r.label, partial, fields };
  });
  return { groupBy, cells };
}

const PARTIAL_MARK = '*';
const DASH = '—';
const NO_RELATIONSHIPS_MESSAGE =
  'no SessionRelationshipRecord rows found for the matched slice; ingest a session with execution-graph wiring or run `burn state rebuild` once relationship backfill is available';

const RELATIONSHIP_ORDER: RelationshipType[] = [
  'root',
  'continuation',
  'fork',
  'subagent',
];

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

function renderNoRelationships(args: ParsedArgs): number {
  if (args.flags['json'] === true) {
    process.stdout.write(
      JSON.stringify(
        {
          relationships: [],
          message: NO_RELATIONSHIPS_MESSAGE,
        },
        null,
        2,
      ) + '\n',
    );
  } else {
    process.stdout.write(`${NO_RELATIONSHIPS_MESSAGE}\n`);
  }
  return 0;
}

/**
 * Load the turns slice that drives every summary mode.
 *
 * Default path: bring `archive.sqlite` current via `buildArchive()` (cheap
 * incremental tail scan after `ingestAll`'s appends), then issue SQL with
 * filters lowered as `WHERE` clauses against indexed columns. Replaces the
 * full ledger walk (`queryAll`) on the hot path.
 *
 * Fallback path: `--no-archive` flag or `RELAYBURN_ARCHIVE=0` env reverts
 * to the legacy `queryAll` ledger stream — kept as an escape hatch for
 * parity validation and for environments where the archive is missing /
 * corrupt. If a build/query against the archive throws, we transparently
 * fall back to the same legacy path so a wedged archive can never break
 * the command.
 */
async function loadTurns(q: Query, args: ParsedArgs): Promise<EnrichedTurn[]> {
  const noArchiveFlag = args.flags['no-archive'] === true;
  const envDisabled = process.env['RELAYBURN_ARCHIVE'] === '0';
  if (noArchiveFlag || envDisabled) {
    return withProgress('reading ledger turns', async (task) => {
      const turns = await queryAll(q);
      task.succeed(`read ${formatInt(turns.length)} ledger turn${turns.length === 1 ? '' : 's'}`);
      return turns;
    });
  }
  try {
    await withProgress('updating archive', async (task) => {
      const result = await buildArchive();
      task.succeed(
        `updated archive: ${formatInt(result.turnsApplied)} turn` +
          `${result.turnsApplied === 1 ? '' : 's'} applied`,
      );
    });
    return await withProgress('querying archive', async (task) => {
      const turns = await queryAllFromArchive(q);
      task.succeed(`read ${formatInt(turns.length)} archive turn${turns.length === 1 ? '' : 's'}`);
      return turns;
    });
  } catch (err) {
    // Don't let an archive-side failure (corrupt sqlite, schema mismatch we
    // didn't recover from cleanly, etc.) take down `burn summary`. Surface
    // the reason on stderr and fall back to the streaming reader.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`burn: archive read failed (${msg}); falling back to ledger walk\n`);
    return withProgress('reading ledger turns', async (task) => {
      const turns = await queryAll(q);
      task.succeed(`read ${formatInt(turns.length)} ledger turn${turns.length === 1 ? '' : 's'}`);
      return turns;
    });
  }
}

function aggregateByModel(turns: EnrichedTurn[], pricing: Parameters<typeof costForTurn>[1]): ModelRow[] {
  return aggregateTurns(turns, pricing, (t) => t.model || 'unknown');
}

function aggregateTurns(
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
  keyForTurn: (turn: EnrichedTurn) => string,
): ModelRow[] {
  const byModel = new Map<string, ModelRow>();
  for (const t of turns) {
    const key = keyForTurn(t) || 'unknown';
    let row = byModel.get(key);
    if (!row) {
      row = emptyRow(key);
      byModel.set(key, row);
    }
    row.turns++;
    row.usage.input += t.usage.input;
    row.usage.output += t.usage.output;
    row.usage.reasoning += t.usage.reasoning;
    row.usage.cacheRead += t.usage.cacheRead;
    row.usage.cacheCreate5m += t.usage.cacheCreate5m;
    row.usage.cacheCreate1h += t.usage.cacheCreate1h;
    accumulateCoverage(row.coverage, t.fidelity?.coverage);
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

function relationshipQueryForTurnSlice(q: Query): Query {
  // Project/workflow/agent/provider filters have already been applied to the
  // turn slice. Let matching turns, not relationship timestamps, define the
  // slice so an old root row can still classify a recent turn in the same
  // session.
  const out: Query = {};
  if (q.sessionId !== undefined) out.sessionId = q.sessionId;
  if (q.source !== undefined) out.source = q.source;
  return out;
}

interface RelationshipTurnIndex {
  allBySession: Map<string, EnrichedTurn[]>;
  mainBySession: Map<string, EnrichedTurn[]>;
  sidechainBySession: Map<string, EnrichedTurn[]>;
  subagentBySessionAgent: Map<string, EnrichedTurn[]>;
}

interface RelationshipMatch {
  relationshipType: RelationshipType;
  sessionId: string;
  subagentType?: string;
  turnCount: number;
  cost: number;
}

interface RelationshipStats {
  relationshipType: RelationshipType;
  count: number;
  sessionCount: number;
  turnCount: number;
  totalCost: number;
  medianCost: number;
  p95Cost: number;
  meanCost: number;
}

interface RelationshipSubagentStats {
  subagentType: string;
  invocations: number;
  turns: number;
  totalCost: number;
  medianCost: number;
  p95Cost: number;
  meanCost: number;
}

function matchRelationshipsToTurns(
  relationships: SessionRelationshipRecord[],
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
): RelationshipMatch[] {
  const index = buildRelationshipTurnIndex(turns);
  const out: RelationshipMatch[] = [];
  const seen = new Set<string>();
  for (const r of relationships) {
    const key = relationshipInstanceKey(r);
    if (seen.has(key)) continue;
    seen.add(key);
    const matchedTurns = turnsForRelationship(r, index);
    if (matchedTurns.length === 0) continue;
    const match: RelationshipMatch = {
      relationshipType: r.relationshipType,
      sessionId: r.sessionId,
      turnCount: matchedTurns.length,
      cost: matchedTurns.reduce((sum, t) => sum + (costForTurn(t, pricing)?.total ?? 0), 0),
    };
    const subagentType = relationshipSubagentType(r, matchedTurns);
    if (subagentType !== undefined) match.subagentType = subagentType;
    out.push(match);
  }
  return out;
}

function buildRelationshipTurnIndex(turns: EnrichedTurn[]): RelationshipTurnIndex {
  const allBySession = new Map<string, EnrichedTurn[]>();
  const mainBySession = new Map<string, EnrichedTurn[]>();
  const sidechainBySession = new Map<string, EnrichedTurn[]>();
  const subagentBySessionAgent = new Map<string, EnrichedTurn[]>();
  for (const turn of turns) {
    pushMap(allBySession, turn.sessionId, turn);
    if (isMainThreadTurn(turn)) pushMap(mainBySession, turn.sessionId, turn);
    if (turn.subagent?.isSidechain) pushMap(sidechainBySession, turn.sessionId, turn);
    const agentId = turn.subagent?.agentId;
    if (agentId !== undefined && agentId.length > 0) {
      pushMap(subagentBySessionAgent, sessionAgentKey(turn.sessionId, agentId), turn);
    }
  }
  return { allBySession, mainBySession, sidechainBySession, subagentBySessionAgent };
}

function turnsForRelationship(
  r: SessionRelationshipRecord,
  index: RelationshipTurnIndex,
): EnrichedTurn[] {
  switch (r.relationshipType) {
    case 'root':
      return index.mainBySession.get(r.sessionId) ?? [];
    case 'subagent': {
      if (r.agentId !== undefined && r.agentId.length > 0) {
        const direct = index.subagentBySessionAgent.get(sessionAgentKey(r.sessionId, r.agentId));
        if (direct !== undefined && direct.length > 0) return direct;
        // Some sources model a spawned agent as its own session and do not
        // annotate the child turns with `subagent`. In that shape the
        // relationship row's sessionId/agentId is the only join key.
        if (r.sessionId === r.agentId) return index.allBySession.get(r.sessionId) ?? [];
      }
      const sidechain = index.sidechainBySession.get(r.sessionId);
      if (sidechain !== undefined && sidechain.length > 0) return sidechain;
      if (r.source === 'spawn-env') return index.allBySession.get(r.sessionId) ?? [];
      return [];
    }
    case 'continuation':
    case 'fork':
      return index.allBySession.get(r.sessionId) ?? [];
  }
}

function aggregateRelationshipStats(matches: RelationshipMatch[]): RelationshipStats[] {
  const byType = new Map<RelationshipType, Map<string, { turns: number; cost: number }>>();
  for (const match of matches) {
    let bySession = byType.get(match.relationshipType);
    if (!bySession) {
      bySession = new Map();
      byType.set(match.relationshipType, bySession);
    }
    const current = bySession.get(match.sessionId) ?? { turns: 0, cost: 0 };
    current.turns += match.turnCount;
    current.cost += match.cost;
    bySession.set(match.sessionId, current);
  }

  const out: RelationshipStats[] = [];
  for (const relationshipType of RELATIONSHIP_ORDER) {
    const bySession = byType.get(relationshipType);
    if (!bySession || bySession.size === 0) continue;
    const costs = [...bySession.values()].map((v) => v.cost).sort((a, b) => a - b);
    const totalCost = costs.reduce((sum, n) => sum + n, 0);
    const sessionCount = bySession.size;
    out.push({
      relationshipType,
      count: sessionCount,
      sessionCount,
      turnCount: [...bySession.values()].reduce((sum, v) => sum + v.turns, 0),
      totalCost,
      medianCost: percentile(costs, 0.5),
      p95Cost: percentile(costs, 0.95),
      meanCost: sessionCount > 0 ? totalCost / sessionCount : 0,
    });
  }
  return out;
}

function aggregateRelationshipSubagentStats(
  matches: RelationshipMatch[],
): RelationshipSubagentStats[] {
  const byType = new Map<string, { turns: number; total: number; costs: number[] }>();
  for (const match of matches) {
    if (match.relationshipType !== 'subagent') continue;
    const type = match.subagentType ?? '(unknown)';
    const current = byType.get(type) ?? { turns: 0, total: 0, costs: [] };
    current.turns += match.turnCount;
    current.total += match.cost;
    current.costs.push(match.cost);
    byType.set(type, current);
  }
  const out: RelationshipSubagentStats[] = [];
  for (const [subagentType, agg] of byType) {
    agg.costs.sort((a, b) => a - b);
    out.push({
      subagentType,
      invocations: agg.costs.length,
      turns: agg.turns,
      totalCost: agg.total,
      medianCost: percentile(agg.costs, 0.5),
      p95Cost: percentile(agg.costs, 0.95),
      meanCost: agg.costs.length > 0 ? agg.total / agg.costs.length : 0,
    });
  }
  return out.sort((a, b) => b.totalCost - a.totalCost);
}

function relationshipSubagentType(
  relationship: SessionRelationshipRecord,
  turns: EnrichedTurn[],
): string | undefined {
  if (relationship.subagentType !== undefined) return relationship.subagentType;
  for (const turn of turns) {
    if (turn.subagent?.subagentType !== undefined) return turn.subagent.subagentType;
  }
  return undefined;
}

function relationshipInstanceKey(r: SessionRelationshipRecord): string {
  return [
    r.source,
    r.relationshipType,
    r.sessionId,
    r.relatedSessionId ?? '',
    r.agentId ?? '',
    r.parentToolUseId ?? '',
  ].join('\0');
}

function sessionAgentKey(sessionId: string, agentId: string): string {
  return `${sessionId}\0${agentId}`;
}

function isMainThreadTurn(turn: EnrichedTurn): boolean {
  const sub = turn.subagent;
  return !sub || !sub.isSidechain || sub.agentId === turn.sessionId;
}

function pushMap<K, V>(map: Map<K, V[]>, key: K, value: V): void {
  const list = map.get(key);
  if (list) list.push(value);
  else map.set(key, [value]);
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const rank = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1));
  return sorted[rank]!;
}

// A turn from before fidelity metadata existed has no `coverage` map; the long-standing
// stance is to treat that as best-effort full fidelity. So if `coverage` is
// undefined every field counts as `known`. When it is present, a field is
// `missing` exactly when its flag is false — same predicate `summarizeFidelity`
// uses against `missingCoverage`.
function accumulateCoverage(target: RowCoverage, coverage: Coverage | undefined): void {
  for (const f of COVERAGE_FIELDS) {
    if (!coverage || coverage[COVERAGE_FLAG[f]]) target[f].known++;
    else target[f].missing++;
  }
}

function emptyRow(label: string): ModelRow {
  return {
    label,
    turns: 0,
    usage: { input: 0, output: 0, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
    cost: { model: label, total: 0, input: 0, output: 0, reasoning: 0, cacheRead: 0, cacheCreate: 0 },
    coverage: {
      input: { known: 0, missing: 0 },
      output: { known: 0, missing: 0 },
      reasoning: { known: 0, missing: 0 },
      cacheRead: { known: 0, missing: 0 },
      cacheCreate: { known: 0, missing: 0 },
    },
  };
}
