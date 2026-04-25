import type { SourceKind, TurnRecord, Usage } from '@relayburn/reader';

import type { ModelCost, PricingTable, ReasoningMode } from './pricing.js';

export interface CostBreakdown {
  model: string;
  total: number;
  input: number;
  output: number;
  reasoning: number;
  cacheRead: number;
  cacheCreate: number;
}

export interface CostForUsageOptions {
  /**
   * Override the reasoning-billing semantics for this call. When omitted, the
   * mode is taken from the resolved `ModelCost` (`reasoningMode`). When given,
   * it wins — used by `costForTurn` to force `included_in_output` for sources
   * (e.g. Codex) whose transcripts already fold reasoning into `output_tokens`.
   */
  reasoningMode?: ReasoningMode;
}

const PER_MILLION = 1_000_000;

export function costForUsage(
  usage: Usage,
  model: string,
  pricing: PricingTable,
  options: CostForUsageOptions = {},
): CostBreakdown | null {
  const rate = lookup(model, pricing);
  if (!rate) return null;
  const mode: ReasoningMode = options.reasoningMode ?? rate.reasoningMode;
  const input = (usage.input / PER_MILLION) * rate.input;
  const output = (usage.output / PER_MILLION) * rate.output;
  const reasoning = reasoningCost(usage.reasoning, rate, mode);
  const cacheRead = (usage.cacheRead / PER_MILLION) * rate.cacheRead;
  const cacheCreate =
    ((usage.cacheCreate5m + usage.cacheCreate1h) / PER_MILLION) * rate.cacheWrite;
  return {
    model,
    total: input + output + reasoning + cacheRead + cacheCreate,
    input,
    output,
    reasoning,
    cacheRead,
    cacheCreate,
  };
}

export function costForTurn(turn: TurnRecord, pricing: PricingTable): CostBreakdown | null {
  const override = reasoningModeForSource(turn.source);
  const opts: CostForUsageOptions = override ? { reasoningMode: override } : {};
  return costForUsage(turn.usage, turn.model, pricing, opts);
}

function reasoningCost(reasoningTokens: number, rate: ModelCost, mode: ReasoningMode): number {
  switch (mode) {
    case 'included_in_output':
      // Already billed inside `usage.output` — informational only.
      return 0;
    case 'separate':
      // Use the model's distinct reasoning tariff. If the override forced this
      // mode but the model has no `rate.reasoning`, fall back to the output
      // rate so we never silently drop reasoning tokens.
      return (reasoningTokens / PER_MILLION) * (rate.reasoning ?? rate.output);
    case 'same_as_output':
    default:
      return (reasoningTokens / PER_MILLION) * rate.output;
  }
}

/**
 * Per-source reasoning-billing semantics override. Returning `undefined` means
 * "defer to the model's `reasoningMode`".
 *
 * - Codex: `output_tokens` already includes reasoning; never bill it on top.
 *   See `../research/ccusage/apps/codex/src/data-loader.ts` for prior art.
 * - Everyone else: defer to the model.
 */
function reasoningModeForSource(source: SourceKind): ReasoningMode | undefined {
  if (source === 'codex') return 'included_in_output';
  return undefined;
}

function lookup(model: string, pricing: PricingTable): ModelCost | undefined {
  const direct = pricing[model];
  if (direct) return direct;
  const stripped = stripProviderPrefix(model);
  if (stripped !== model) {
    const viaStripped = pricing[stripped];
    if (viaStripped) return viaStripped;
  }
  return undefined;
}

function stripProviderPrefix(model: string): string {
  const i = model.indexOf('/');
  return i >= 0 ? model.slice(i + 1) : model;
}

export function sumCosts(costs: CostBreakdown[]): CostBreakdown {
  const total = costs.reduce(
    (a, c) => ({
      model: 'aggregate',
      total: a.total + c.total,
      input: a.input + c.input,
      output: a.output + c.output,
      reasoning: a.reasoning + c.reasoning,
      cacheRead: a.cacheRead + c.cacheRead,
      cacheCreate: a.cacheCreate + c.cacheCreate,
    }),
    {
      model: 'aggregate',
      total: 0,
      input: 0,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate: 0,
    },
  );
  return total;
}
