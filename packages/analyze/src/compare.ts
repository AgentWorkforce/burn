import type { EnrichedTurn } from '@relayburn/ledger';
import type { ActivityCategory } from '@relayburn/reader';

import { costForTurn } from './cost.js';
import type { PricingTable } from './pricing.js';

export interface CompareCell {
  turns: number;
  editTurns: number;
  oneShotTurns: number;
  totalCost: number;
  costPerTurn: number | null;
  oneShotRate: number | null;
  cacheHitRate: number | null;
  medianRetries: number | null;
  insufficientSample: boolean;
}

export interface CompareTable {
  models: string[];
  categories: string[];
  cells: Record<string, Record<string, CompareCell>>;
  totals: Record<string, { turns: number; totalCost: number }>;
  minSample: number;
}

export interface CompareOptions {
  pricing: PricingTable;
  models?: string[];
  minSample?: number;
}

export const DEFAULT_MIN_SAMPLE = 5;

interface Accum {
  turns: number;
  editTurns: number;
  oneShotTurns: number;
  totalCost: number;
  retriesSamples: number[];
  cacheRead: number;
  tokenDenominator: number;
}

export function buildCompareTable(turns: EnrichedTurn[], opts: CompareOptions): CompareTable {
  const minSample = opts.minSample ?? DEFAULT_MIN_SAMPLE;
  const modelFilter = opts.models && opts.models.length > 0 ? new Set(opts.models) : null;

  const byModelCategory = new Map<string, Map<string, Accum>>();
  const modelTotals = new Map<string, { turns: number; totalCost: number }>();
  const modelSet = new Set<string>();
  const categorySet = new Set<string>();

  for (const t of turns) {
    const model = t.model || 'unknown';
    if (modelFilter && !modelFilter.has(model)) continue;
    const cat = (t.activity as string | undefined) ?? 'unclassified';
    modelSet.add(model);
    categorySet.add(cat);

    let byCat = byModelCategory.get(model);
    if (!byCat) {
      byCat = new Map();
      byModelCategory.set(model, byCat);
    }
    let acc = byCat.get(cat);
    if (!acc) {
      acc = newAccum();
      byCat.set(cat, acc);
    }
    acc.turns++;
    const mt = modelTotals.get(model) ?? { turns: 0, totalCost: 0 };
    mt.turns++;
    const c = costForTurn(t, opts.pricing);
    if (c) {
      acc.totalCost += c.total;
      mt.totalCost += c.total;
    }
    modelTotals.set(model, mt);
    if (t.hasEdits) {
      acc.editTurns++;
      const r = t.retries ?? 0;
      acc.retriesSamples.push(r);
      if (r === 0) acc.oneShotTurns++;
    }
    acc.cacheRead += t.usage.cacheRead;
    acc.tokenDenominator +=
      t.usage.input + t.usage.cacheRead + t.usage.cacheCreate5m + t.usage.cacheCreate1h;
  }

  const models = [...modelSet].sort((a, b) => {
    const ca = modelTotals.get(a)?.totalCost ?? 0;
    const cb = modelTotals.get(b)?.totalCost ?? 0;
    if (cb !== ca) return cb - ca;
    return a.localeCompare(b);
  });
  const categories = [...categorySet].sort((a, b) => {
    let ta = 0;
    let tb = 0;
    for (const m of models) {
      ta += byModelCategory.get(m)?.get(a)?.turns ?? 0;
      tb += byModelCategory.get(m)?.get(b)?.turns ?? 0;
    }
    if (tb !== ta) return tb - ta;
    return a.localeCompare(b);
  });

  const cells: CompareTable['cells'] = {};
  for (const m of models) {
    cells[m] = {};
    for (const cat of categories) {
      cells[m]![cat] = toCell(byModelCategory.get(m)?.get(cat), minSample);
    }
  }

  const totals: CompareTable['totals'] = {};
  for (const [m, v] of modelTotals) totals[m] = v;

  return { models, categories, cells, totals, minSample };
}

function toCell(acc: Accum | undefined, minSample: number): CompareCell {
  if (!acc || acc.turns === 0) {
    return {
      turns: 0,
      editTurns: 0,
      oneShotTurns: 0,
      totalCost: 0,
      costPerTurn: null,
      oneShotRate: null,
      cacheHitRate: null,
      medianRetries: null,
      insufficientSample: true,
    };
  }
  return {
    turns: acc.turns,
    editTurns: acc.editTurns,
    oneShotTurns: acc.oneShotTurns,
    totalCost: acc.totalCost,
    costPerTurn: acc.totalCost / acc.turns,
    oneShotRate: acc.editTurns > 0 ? acc.oneShotTurns / acc.editTurns : null,
    cacheHitRate: acc.tokenDenominator > 0 ? acc.cacheRead / acc.tokenDenominator : null,
    medianRetries: acc.editTurns > 0 ? median(acc.retriesSamples) : null,
    insufficientSample: acc.turns < minSample,
  };
}

function newAccum(): Accum {
  return {
    turns: 0,
    editTurns: 0,
    oneShotTurns: 0,
    totalCost: 0,
    retriesSamples: [],
    cacheRead: 0,
    tokenDenominator: 0,
  };
}

function median(xs: number[]): number {
  if (xs.length === 0) return 0;
  const s = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 === 0 ? (s[mid - 1]! + s[mid]!) / 2 : s[mid]!;
}

// Re-exported for completeness; callers can narrow when they know every turn
// was classified.
export type CompareCategory = ActivityCategory | 'unclassified';
