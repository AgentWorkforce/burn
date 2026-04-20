import { loadPricing, costForTurn } from '@relayburn/analyze';
import { queryAll, type Query } from '@relayburn/ledger';
import type { EnrichedTurn } from '@relayburn/ledger';

import { ingestClaudeProjects } from '../ingest.js';
import { formatInt, formatUsd, parseSinceArg, table } from '../format.js';
import type { ParsedArgs } from '../args.js';

export async function runByTool(args: ParsedArgs): Promise<number> {
  const q: Query = {};
  if (typeof args.flags['since'] === 'string') q.since = parseSinceArg(args.flags['since']);
  if (typeof args.flags['project'] === 'string') q.project = args.flags['project'];
  if (typeof args.flags['session'] === 'string') q.sessionId = args.flags['session'];

  await ingestClaudeProjects();
  const pricing = await loadPricing();
  const turns = await queryAll(q);

  const { byTool, unattributed } = attributeCostToTools(turns, pricing);

  const rows: string[][] = [['tool', 'calls', 'attributedCost']];
  const sorted = [...byTool.entries()].sort((a, b) => b[1].cost - a[1].cost);
  for (const [tool, { calls, cost }] of sorted) {
    rows.push([tool, formatInt(calls), formatUsd(cost)]);
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
  out.push(table(rows));
  out.push('');
  out.push(
    `attributedCost = (turn N input cost) split evenly across tool_use blocks in turn N-1, grouped by tool name.`,
  );
  out.push(`unattributed cost (no prior tool call, e.g. first turn): ${formatUsd(unattributed)}`);
  out.push('');

  process.stdout.write(out.join('\n'));
  return 0;
}

interface ToolAgg {
  calls: number;
  cost: number;
}

function attributeCostToTools(
  turns: EnrichedTurn[],
  pricing: Parameters<typeof costForTurn>[1],
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
    for (let i = 0; i < list.length; i++) {
      const turn = list[i]!;
      const c = costForTurn(turn, pricing);
      if (!c) continue;
      // cost this turn paid to ingest the PRIOR turn's tool outputs:
      const ingestCost = c.input + c.cacheRead + c.cacheCreate;

      // Also count the tool-calls this turn emits (so they appear even if the next turn is unpriced).
      for (const tc of turn.toolCalls) {
        const agg = byTool.get(tc.name) ?? { calls: 0, cost: 0 };
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
      const share = ingestCost / prior.toolCalls.length;
      for (const tc of prior.toolCalls) {
        const agg = byTool.get(tc.name) ?? { calls: 0, cost: 0 };
        agg.cost += share;
        byTool.set(tc.name, agg);
      }
    }
  }

  return { byTool, unattributed };
}
