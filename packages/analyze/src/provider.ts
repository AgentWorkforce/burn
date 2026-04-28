import type { SourceKind, TurnRecord, Usage, Coverage } from '@relayburn/reader';

import { costForTurn } from './cost.js';
import type { CostBreakdown } from './cost.js';
import type { PricingTable } from './pricing.js';
import {
  DEFAULT_RULES,
  resolveProvider,
  type ProviderRule,
} from './provider-reattribution.js';

export interface TurnProvider {
  provider: string;
  rawModel: string;
  normalizedModel: string;
  matchedRule?: string;
}

export type ProviderFilter = ReadonlySet<string>;

export interface FieldCoverage {
  known: number;
  missing: number;
}

export type CoverageField =
  | 'input'
  | 'output'
  | 'reasoning'
  | 'cacheRead'
  | 'cacheCreate';

export type RowCoverage = Record<CoverageField, FieldCoverage>;

export interface UsageCostAggregateRow {
  label: string;
  turns: number;
  usage: Usage;
  cost: CostBreakdown;
  coverage: RowCoverage;
}

export interface ProviderAggregateRow extends UsageCostAggregateRow {
  provider: string;
}

export interface AggregateByProviderOptions {
  pricing: PricingTable;
  rules?: readonly ProviderRule[];
}

/**
 * Resolve the effective provider for a turn.
 *
 * Synthetic-style router prefixes win first. Otherwise we keep provider
 * semantics aligned with CLI rendering by using a raw `provider/model` model
 * prefix when present, then falling back to the collector-implied provider.
 */
export function providerFor(
  turn: Pick<TurnRecord, 'model' | 'source'>,
  rules: readonly ProviderRule[] = DEFAULT_RULES,
): TurnProvider {
  return providerForModel(turn.model, turn.source, rules);
}

export const providerForTurn = providerFor;
export const resolveTurnProvider = providerFor;

export function providerForModel(
  model: string,
  source?: SourceKind,
  rules: readonly ProviderRule[] = DEFAULT_RULES,
): TurnProvider {
  const resolved = resolveProvider(model, rules);
  if (resolved.provider) {
    const out: TurnProvider = {
      provider: resolved.provider,
      rawModel: model,
      normalizedModel: resolved.normalizedModel,
    };
    if (resolved.matchedRule) out.matchedRule = resolved.matchedRule;
    return out;
  }

  const providerPrefix = providerFromModelPrefix(model);
  if (providerPrefix) {
    return {
      provider: providerPrefix,
      rawModel: model,
      normalizedModel: stripProviderPrefix(model),
    };
  }

  return {
    provider: source ? providerFromSource(source) : 'unknown',
    rawModel: model,
    normalizedModel: model,
  };
}

export function filterTurnsByProvider<T extends Pick<TurnRecord, 'model' | 'source'>>(
  turns: T[],
  filter: ProviderFilter | undefined,
  rules: readonly ProviderRule[] = DEFAULT_RULES,
): T[] {
  if (!filter) return turns;
  return turns.filter((t) => filter.has(providerFor(t, rules).provider.toLowerCase()));
}

export function aggregateByProvider(
  turns: readonly TurnRecord[],
  opts: AggregateByProviderOptions,
): ProviderAggregateRow[] {
  const byProvider = new Map<string, ProviderAggregateRow>();
  const rules = opts.rules ?? DEFAULT_RULES;
  for (const t of turns) {
    const provider = providerFor(t, rules).provider || 'unknown';
    let row = byProvider.get(provider);
    if (!row) {
      row = emptyProviderRow(provider);
      byProvider.set(provider, row);
    }
    row.turns++;
    row.usage.input += t.usage.input;
    row.usage.output += t.usage.output;
    row.usage.reasoning += t.usage.reasoning;
    row.usage.cacheRead += t.usage.cacheRead;
    row.usage.cacheCreate5m += t.usage.cacheCreate5m;
    row.usage.cacheCreate1h += t.usage.cacheCreate1h;
    accumulateCoverage(row.coverage, t.fidelity?.coverage);
    const c = costForTurn(t, opts.pricing);
    if (c) {
      row.cost.total += c.total;
      row.cost.input += c.input;
      row.cost.output += c.output;
      row.cost.reasoning += c.reasoning;
      row.cost.cacheRead += c.cacheRead;
      row.cost.cacheCreate += c.cacheCreate;
    }
  }
  return [...byProvider.values()].sort((a, b) => b.cost.total - a.cost.total);
}

function emptyProviderRow(provider: string): ProviderAggregateRow {
  return {
    label: provider,
    provider,
    turns: 0,
    usage: {
      input: 0,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    cost: {
      model: provider,
      total: 0,
      input: 0,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate: 0,
    },
    coverage: emptyCoverage(),
  };
}

function emptyCoverage(): RowCoverage {
  return {
    input: { known: 0, missing: 0 },
    output: { known: 0, missing: 0 },
    reasoning: { known: 0, missing: 0 },
    cacheRead: { known: 0, missing: 0 },
    cacheCreate: { known: 0, missing: 0 },
  };
}

function accumulateCoverage(target: RowCoverage, coverage: Coverage | undefined): void {
  for (const f of COVERAGE_FIELDS) {
    if (!coverage || coverage[COVERAGE_FLAG[f]]) target[f].known++;
    else target[f].missing++;
  }
}

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

function providerFromModelPrefix(model: string): string | undefined {
  const i = model.indexOf('/');
  if (i <= 0) return undefined;
  return model.slice(0, i).toLowerCase();
}

function stripProviderPrefix(model: string): string {
  const i = model.indexOf('/');
  return i >= 0 ? model.slice(i + 1) : model;
}

function providerFromSource(source: SourceKind): string {
  switch (source) {
    case 'claude-code':
    case 'anthropic-api':
      return 'anthropic';
    case 'codex':
    case 'openai-api':
      return 'openai';
    case 'gemini-api':
      return 'google';
    case 'opencode':
    default:
      return source;
  }
}
