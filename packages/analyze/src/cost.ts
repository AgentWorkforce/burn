import type { TurnRecord, Usage } from '@relayburn/reader';

import type { ModelCost, PricingTable } from './pricing.js';

export interface CostBreakdown {
  model: string;
  total: number;
  input: number;
  output: number;
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
  const output = ((usage.output + usage.reasoning) / PER_MILLION) * rate.output;
  const cacheRead = (usage.cacheRead / PER_MILLION) * rate.cacheRead;
  const cacheCreate =
    ((usage.cacheCreate5m + usage.cacheCreate1h) / PER_MILLION) * rate.cacheWrite;
  return {
    model,
    total: input + output + cacheRead + cacheCreate,
    input,
    output,
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
      cacheRead: a.cacheRead + c.cacheRead,
      cacheCreate: a.cacheCreate + c.cacheCreate,
    }),
    { model: 'aggregate', total: 0, input: 0, output: 0, cacheRead: 0, cacheCreate: 0 },
  );
  return total;
}
