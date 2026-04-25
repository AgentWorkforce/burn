import type { TurnRecord, Usage } from '@relayburn/reader';

import type { ModelCost, PricingTable } from './pricing.js';
import { resolveProvider } from './provider-reattribution.js';

export interface CostBreakdown {
  model: string;
  total: number;
  input: number;
  output: number;
  reasoning: number;
  cacheRead: number;
  cacheCreate: number;
}

const PER_MILLION = 1_000_000;

export function costForUsage(
  usage: Usage,
  model: string,
  pricing: PricingTable,
): CostBreakdown | null {
  const rate = lookup(model, pricing);
  if (!rate) return null;
  const input = (usage.input / PER_MILLION) * rate.input;
  const output = (usage.output / PER_MILLION) * rate.output;
  const reasoning = (usage.reasoning / PER_MILLION) * rate.output;
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
  return costForUsage(turn.usage, turn.model, pricing);
}

function lookup(model: string, pricing: PricingTable): ModelCost | undefined {
  const direct = pricing[model];
  if (direct) return direct;
  // Reattribution layer (issue #31): Synthetic-routed model IDs carry prefixes
  // like `hf:deepseek-ai/...` or `accounts/fireworks/models/...` that don't
  // match models.dev. Strip the routing prefix and try the residual id so the
  // underlying model's price applies whether the call went direct or through
  // the aggregator.
  const reattributed = resolveProvider(model);
  if (reattributed.normalizedModel !== model) {
    const viaReattributed = pricing[reattributed.normalizedModel];
    if (viaReattributed) return viaReattributed;
  }
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
