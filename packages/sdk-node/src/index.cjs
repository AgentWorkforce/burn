// CommonJS variant of the umbrella facade. Required for tools that resolve
// `require('@relayburn/sdk')` through CJS — the ESM `src/index.js` is the
// canonical entry point, but Node falls back to this when a consumer's
// package is `"type": "commonjs"` and the `exports.require` map is honored.
//
// Mirrors the ESM facade verb-for-verb. The sync binding verbs are wrapped
// in `async` so callers see `Promise<T>` (matching the Node facade contract);
// see `src/index.js` for the rationale.

'use strict';

const binding = require('./binding.cjs');

// See `src/index.js` for the rationale: napi-rs serializes Rust `u64` /
// `i64` as JS `BigInt`. We downcast safe-range BigInts to match common
// JavaScript caller expectations while keeping larger values precise.
const MIN_SAFE = BigInt(Number.MIN_SAFE_INTEGER);
const MAX_SAFE = BigInt(Number.MAX_SAFE_INTEGER);

function coerceBigInts(value) {
  if (typeof value === 'bigint') {
    return value >= MIN_SAFE && value <= MAX_SAFE ? Number(value) : value;
  }
  if (Array.isArray(value)) {
    for (let i = 0; i < value.length; i++) {
      value[i] = coerceBigInts(value[i]);
    }
    return value;
  }
  if (value !== null && typeof value === 'object') {
    const proto = Object.getPrototypeOf(value);
    if (proto === null || proto === Object.prototype) {
      for (const key of Object.keys(value)) {
        value[key] = coerceBigInts(value[key]);
      }
    }
    return value;
  }
  return value;
}

function normalizeSearchOptions(opts) {
  if (!opts || typeof opts !== 'object' || typeof opts.limit !== 'number') {
    return opts;
  }
  if (!Number.isSafeInteger(opts.limit) || opts.limit < 0) {
    throw new RangeError('search limit must be a non-negative safe integer');
  }
  return { ...opts, limit: BigInt(opts.limit) };
}

class Ledger {
  constructor(home) {
    this.home = home;
  }
  static async open(opts) {
    const home = binding.ledgerOpen(opts);
    return new Ledger(home);
  }
}

function computeCompareExcluded(summary, minimum) {
  const out = { total: 0, aggregateOnly: 0, costOnly: 0, partial: 0, usageOnly: 0 };
  if (minimum === 'partial') return out;
  const order = ['cost-only', 'aggregate-only', 'partial', 'usage-only', 'full'];
  const need = order.indexOf(minimum);
  if (need < 0) {
    throw new Error(
      `invalid minimum fidelity: ${minimum} (expected one of ${order.join(', ')})`,
    );
  }
  const byClass = summary?.byClass ?? {};
  for (const cls of order) {
    if (order.indexOf(cls) >= need) continue;
    const n = Number(byClass[cls] ?? 0);
    if (!n) continue;
    out.total += n;
    if (cls === 'aggregate-only') out.aggregateOnly += n;
    else if (cls === 'cost-only') out.costOnly += n;
    else if (cls === 'partial') out.partial += n;
    else if (cls === 'usage-only') out.usageOnly += n;
  }
  return out;
}

module.exports = {
  Ledger,
  ingest: async (opts) => coerceBigInts(await binding.ingest(opts)),
  summary: async (opts) => coerceBigInts(await binding.summary(opts)),
  summaryReport: async (opts) => coerceBigInts(await binding.summaryReport(opts)),
  summaryTimeseries: async (opts) => coerceBigInts(await binding.summaryTimeseries(opts)),
  capabilities: () => coerceBigInts(binding.capabilities()),
  sessionCost: async (opts) => coerceBigInts(await binding.sessionCost(opts)),
  fingerprint: async (opts) => coerceBigInts(await binding.fingerprint(opts)),
  overhead: async (opts) => coerceBigInts(await binding.overhead(opts)),
  overheadTrim: async (opts) => coerceBigInts(await binding.overheadTrim(opts)),
  hotspots: async (opts) => coerceBigInts(await binding.hotspots(opts)),
  compare: async (opts) => coerceBigInts(await binding.compare(opts)),
  writePendingStamp: async (opts) => coerceBigInts(await binding.writePendingStamp(opts)),
  computeCompareExcluded,
  search: async (opts) => coerceBigInts(await binding.search(normalizeSearchOptions(opts))),
  exportLedger: async (opts) => coerceBigInts(await binding.exportLedger(opts)),
  exportStamps: async (opts) => coerceBigInts(await binding.exportStamps(opts)),
  BurnErrorCode: binding.BurnErrorCode,
  OverheadFileKind: binding.OverheadFileKind,
  HotspotsGroupBy: binding.HotspotsGroupBy,
};
