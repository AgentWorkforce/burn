import type { EnrichedTurn } from '@relayburn/ledger';
import type { ActivityCategory, Coverage, FidelityClass } from '@relayburn/reader';

import { costForTurn } from './cost.js';
import type { PricingTable } from './pricing.js';

export interface CompareCell {
  turns: number;
  editTurns: number;
  oneShotTurns: number;
  // Number of turns whose model had pricing in the active table. When this is
  // less than `turns`, totalCost / costPerTurn under-count what the cell
  // actually consumed.
  pricedTurns: number;
  totalCost: number;
  // null when no priced turns; cells with unpriced models render as "—",
  // never as "$0.00".
  costPerTurn: number | null;
  // null for categories with no edits (exploration, brainstorming, planning,
  // delegation, testing, git, deps, format, build-deploy, verification,
  // review, reasoning, conversation) and for empty cells.
  oneShotRate: number | null;
  cacheHitRate: number | null;
  medianRetries: number | null;
  // True when the cell has zero turns. Distinct from `insufficientSample` so
  // JSON/CSV consumers can tell "we never saw this combination" apart from
  // "we have data but the sample is small." Only one of `noData` /
  // `insufficientSample` is ever true at a time.
  noData: boolean;
  // True when 0 < turns < minSample. A cell with `noData: true` always has
  // `insufficientSample: false`.
  insufficientSample: boolean;
}

export interface CompareTable {
  models: string[];
  categories: string[];
  cells: Record<string, Record<string, CompareCell>>;
  totals: Record<string, { turns: number; pricedTurns: number; totalCost: number }>;
  minSample: number;
  sample: CompareSample;
}

export interface CompareSample {
  totalTurns: number;
  includedTurns: number;
  excludedTurns: number;
  allowedFidelity: FidelityClass[];
  includeUnknownFidelity: boolean;
  unknownFidelityTurns: number;
  excludedByClass: Record<FidelityClass, number>;
}

export interface CompareOptions {
  pricing: PricingTable;
  models?: string[];
  minSample?: number;
  fidelity?: FidelityClass[];
  includePartial?: boolean;
}

export const DEFAULT_MIN_SAMPLE = 5;
export const DEFAULT_COMPARE_FIDELITY: ReadonlyArray<FidelityClass> = [
  'full',
  'usage-only',
];

interface Accum {
  turns: number;
  editTurns: number;
  oneShotTurns: number;
  pricedTurns: number;
  totalCost: number;
  retriesSamples: number[];
  cacheRead: number;
  tokenDenominator: number;
}

export function buildCompareTable(turns: EnrichedTurn[], opts: CompareOptions): CompareTable {
  const minSample = opts.minSample ?? DEFAULT_MIN_SAMPLE;
  const modelFilter = opts.models && opts.models.length > 0 ? new Set(opts.models) : null;
  const allowedFidelity = normalizeAllowedFidelity(opts);
  const allowedSet = new Set<FidelityClass>(allowedFidelity);
  const sample = emptySample(allowedFidelity);

  const byModelCategory = new Map<string, Map<string, Accum>>();
  const modelTotals = new Map<string, { turns: number; pricedTurns: number; totalCost: number }>();
  const modelSet = new Set<string>();
  const categorySet = new Set<string>();

  // Pre-seed modelSet from the --models filter so a model the user explicitly
  // asked about stays visible (as an all-empty column with coverage notes)
  // even if zero turns matched. Without this, the "no <model> data" coverage
  // signal silently disappears for filtered-but-absent models.
  if (modelFilter) {
    for (const m of modelFilter) {
      modelSet.add(m);
      modelTotals.set(m, { turns: 0, pricedTurns: 0, totalCost: 0 });
    }
  }

  for (const t of turns) {
    const model = t.model || 'unknown';
    if (modelFilter && !modelFilter.has(model)) continue;
    sample.totalTurns++;
    if (!isTurnIncludedByFidelity(t, allowedSet, sample)) continue;
    sample.includedTurns++;
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
    const mt = modelTotals.get(model) ?? { turns: 0, pricedTurns: 0, totalCost: 0 };
    mt.turns++;
    const c = hasCostCoverage(t) ? costForTurn(t, opts.pricing) : null;
    if (c) {
      acc.pricedTurns++;
      acc.totalCost += c.total;
      mt.pricedTurns++;
      mt.totalCost += c.total;
    }
    modelTotals.set(model, mt);
    if (t.hasEdits) {
      acc.editTurns++;
      const r = t.retries ?? 0;
      acc.retriesSamples.push(r);
      if (r === 0) acc.oneShotTurns++;
    }
    if (hasCacheHitCoverage(t)) {
      acc.cacheRead += t.usage.cacheRead;
      acc.tokenDenominator +=
        t.usage.input + t.usage.cacheRead + t.usage.cacheCreate5m + t.usage.cacheCreate1h;
    }
  }
  sample.excludedTurns = sample.totalTurns - sample.includedTurns;

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

  return { models, categories, cells, totals, minSample, sample };
}

function normalizeAllowedFidelity(opts: CompareOptions): FidelityClass[] {
  const seen = new Set<FidelityClass>();
  const out: FidelityClass[] = [];
  const requested =
    opts.fidelity && opts.fidelity.length > 0
      ? opts.fidelity
      : DEFAULT_COMPARE_FIDELITY;
  for (const cls of requested) {
    if (!seen.has(cls)) {
      seen.add(cls);
      out.push(cls);
    }
  }
  if (opts.includePartial && !seen.has('partial')) out.push('partial');
  return out;
}

function emptySample(allowedFidelity: FidelityClass[]): CompareSample {
  return {
    totalTurns: 0,
    includedTurns: 0,
    excludedTurns: 0,
    allowedFidelity,
    includeUnknownFidelity: true,
    unknownFidelityTurns: 0,
    excludedByClass: {
      full: 0,
      'usage-only': 0,
      'aggregate-only': 0,
      'cost-only': 0,
      partial: 0,
    },
  };
}

function isTurnIncludedByFidelity(
  turn: EnrichedTurn,
  allowed: ReadonlySet<FidelityClass>,
  sample: CompareSample,
): boolean {
  const fidelity = turn.fidelity;
  if (!fidelity) {
    sample.unknownFidelityTurns++;
    return true;
  }
  if (allowed.has(fidelity.class)) return true;
  sample.excludedByClass[fidelity.class]++;
  return false;
}

function hasCostCoverage(turn: EnrichedTurn): boolean {
  const c = turn.fidelity?.coverage;
  if (!c) return true;
  return c.hasInputTokens && c.hasOutputTokens;
}

function hasCacheHitCoverage(turn: EnrichedTurn): boolean {
  const c = turn.fidelity?.coverage;
  if (!c) return true;
  return c.hasInputTokens && c.hasCacheReadTokens && c.hasCacheCreateTokens;
}

function toCell(acc: Accum | undefined, minSample: number): CompareCell {
  if (!acc || acc.turns === 0) {
    return {
      turns: 0,
      editTurns: 0,
      oneShotTurns: 0,
      pricedTurns: 0,
      totalCost: 0,
      costPerTurn: null,
      oneShotRate: null,
      cacheHitRate: null,
      medianRetries: null,
      noData: true,
      insufficientSample: false,
    };
  }
  return {
    turns: acc.turns,
    editTurns: acc.editTurns,
    oneShotTurns: acc.oneShotTurns,
    pricedTurns: acc.pricedTurns,
    totalCost: acc.totalCost,
    // costPerTurn is null when none of the turns in this cell have pricing —
    // emitting 0 would silently misrepresent unknown cost as free.
    costPerTurn: acc.pricedTurns > 0 ? acc.totalCost / acc.pricedTurns : null,
    oneShotRate: acc.editTurns > 0 ? acc.oneShotTurns / acc.editTurns : null,
    cacheHitRate: acc.tokenDenominator > 0 ? acc.cacheRead / acc.tokenDenominator : null,
    medianRetries: acc.editTurns > 0 ? median(acc.retriesSamples) : null,
    noData: false,
    insufficientSample: acc.turns < minSample,
  };
}

function newAccum(): Accum {
  return {
    turns: 0,
    editTurns: 0,
    oneShotTurns: 0,
    pricedTurns: 0,
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
