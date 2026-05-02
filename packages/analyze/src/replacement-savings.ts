// Estimated tokens saved by replacement tools (e.g. relaywash) that ship a
// `_meta.replaces` / `_meta.collapsedCalls` annotation on their tool_result.
//
// The reader back-populates `replacedTools` and `collapsedCalls` onto each
// `ToolCall` when those annotations are present. This module turns those
// counterfactuals into a tokens-saved estimate using a static lookup table
// keyed by the *replaced* tool name.
//
// The table is intentionally approximate. Per-call costs vary wildly with
// argument shape and result size; absent a per-(tool, project) corpus the
// static numbers are the cheap, deterministic baseline. The accompanying
// issue (#219) calls out option (b) — averaging from the live ledger — as a
// future refinement once we have enough samples.

import type { ToolCall, TurnRecord } from '@relayburn/reader';

// Average tokens (input + output) one vanilla call of each tool consumes.
// Numbers are conservative midpoints derived from common Claude Code session
// shapes — Read of a moderate file, a Grep returning a handful of hits, etc.
// Update with care: `summary --by-tool` and `summary` headlines surface this
// table directly.
export const DEFAULT_REPLACED_TOOL_TOKEN_COST: Readonly<Record<string, number>> = Object.freeze({
  Bash: 600,
  BashOutput: 400,
  Edit: 700,
  Glob: 250,
  Grep: 900,
  KillShell: 100,
  LS: 300,
  MultiEdit: 1100,
  NotebookEdit: 900,
  Read: 2200,
  Task: 5000,
  TodoWrite: 250,
  WebFetch: 3000,
  WebSearch: 2500,
  Write: 1600,
});

// Fallback when a replaced tool isn't in the table. Picked to be in the same
// order of magnitude as the most common entries so unknown names don't skew
// totals dramatically in either direction.
export const DEFAULT_FALLBACK_TOKEN_COST = 800;

export interface ReplacementSavingsOptions {
  // Optional override of the per-tool token cost lookup. Provided values are
  // merged on top of the builtin defaults.
  costPerCall?: Readonly<Record<string, number>>;
  // Optional fallback used when a replaced tool name isn't in the table.
  fallbackCostPerCall?: number;
}

export interface ToolCallSavings {
  collapsedCalls: number;
  replacedTools: string[];
  estimatedTokensSaved: number;
}

export interface ToolSavingsAggregate {
  // Number of *replacement-tool* calls (not collapsed calls) carrying any
  // counterfactual annotation. One row per replacement tool name.
  calls: number;
  collapsedCalls: number;
  estimatedTokensSaved: number;
}

export interface ReplacementSavingsSummary {
  // Total replacement-tool calls (count of ToolCalls carrying any counterfactual).
  calls: number;
  // Sum of `collapsedCalls` across all annotated ToolCalls.
  collapsedCalls: number;
  estimatedTokensSaved: number;
  // Per-replacement-tool aggregate keyed by ToolCall.name.
  byTool: Map<string, ToolSavingsAggregate>;
}

function resolveCostPerCall(
  options: ReplacementSavingsOptions | undefined,
): { table: Readonly<Record<string, number>>; fallback: number } {
  const fallback =
    typeof options?.fallbackCostPerCall === 'number' && options.fallbackCostPerCall >= 0
      ? options.fallbackCostPerCall
      : DEFAULT_FALLBACK_TOKEN_COST;
  if (!options?.costPerCall) {
    return { table: DEFAULT_REPLACED_TOOL_TOKEN_COST, fallback };
  }
  return { table: { ...DEFAULT_REPLACED_TOOL_TOKEN_COST, ...options.costPerCall }, fallback };
}

// Average per-call token cost across the listed replaced tools. Returns the
// fallback when `replaced` is empty so a `collapsedCalls` count alone still
// produces a non-zero estimate.
function averageReplacedCost(
  replaced: readonly string[],
  table: Readonly<Record<string, number>>,
  fallback: number,
): number {
  if (replaced.length === 0) return fallback;
  let total = 0;
  for (const name of replaced) {
    total += table[name] ?? fallback;
  }
  return total / replaced.length;
}

// Per-call savings estimate. Returns undefined for calls without any
// annotation (so callers can skip them in aggregates / displays).
export function estimateSavingsForToolCall(
  call: ToolCall,
  options?: ReplacementSavingsOptions,
): ToolCallSavings | undefined {
  const collapsedCalls = call.collapsedCalls ?? 0;
  const replaced = call.replacedTools ?? [];
  if (collapsedCalls <= 0 && replaced.length === 0) return undefined;
  const { table, fallback } = resolveCostPerCall(options);
  const avg = averageReplacedCost(replaced, table, fallback);
  // When `collapsedCalls` is missing but `replaces` is present, treat the
  // call as having replaced one of each named tool. That's the conservative
  // floor: at least one vanilla call per listed name.
  const calls = collapsedCalls > 0 ? collapsedCalls : replaced.length;
  return {
    collapsedCalls: calls,
    replacedTools: replaced.slice(),
    estimatedTokensSaved: Math.round(calls * avg),
  };
}

export function summarizeReplacementSavings(
  turns: readonly TurnRecord[],
  options?: ReplacementSavingsOptions,
): ReplacementSavingsSummary {
  const byTool = new Map<string, ToolSavingsAggregate>();
  let calls = 0;
  let collapsed = 0;
  let saved = 0;
  for (const turn of turns) {
    for (const tc of turn.toolCalls) {
      const est = estimateSavingsForToolCall(tc, options);
      if (!est) continue;
      calls++;
      collapsed += est.collapsedCalls;
      saved += est.estimatedTokensSaved;
      let agg = byTool.get(tc.name);
      if (!agg) {
        agg = { calls: 0, collapsedCalls: 0, estimatedTokensSaved: 0 };
        byTool.set(tc.name, agg);
      }
      agg.calls++;
      agg.collapsedCalls += est.collapsedCalls;
      agg.estimatedTokensSaved += est.estimatedTokensSaved;
    }
  }
  return { calls, collapsedCalls: collapsed, estimatedTokensSaved: saved, byTool };
}
