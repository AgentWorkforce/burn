// CommonJS variant of the umbrella facade. Required for tools that resolve
// `require('@relayburn/sdk')` through CJS — the ESM `src/index.js` is the
// canonical entry point, but Node falls back to this when a consumer's
// package is `"type": "commonjs"` and the `exports.require` map is honored.
//
// Mirrors the ESM facade verb-for-verb. The sync binding verbs are wrapped
// in `async` so callers see `Promise<T>` (matching the 1.x TS contract);
// see `src/index.js` for the rationale.

'use strict';

const binding = require('./binding.cjs');

// See `src/index.js` for the rationale: napi-rs serializes Rust `u64` /
// `i64` as JS `BigInt`, while TS 1.x `@relayburn/sdk` emits plain
// `Number`. We downcast safe-range BigInts to keep `deepStrictEqual`
// passing in conformance and to match user expectations from 1.x.
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

class Ledger {
  constructor(home) {
    this.home = home;
  }
  static async open(opts) {
    const home = binding.ledgerOpen(opts);
    return new Ledger(home);
  }
}

module.exports = {
  Ledger,
  ingest: async (opts) => coerceBigInts(await binding.ingest(opts)),
  summary: async (opts) => coerceBigInts(await binding.summary(opts)),
  sessionCost: async (opts) => coerceBigInts(await binding.sessionCost(opts)),
  overhead: async (opts) => coerceBigInts(await binding.overhead(opts)),
  overheadTrim: async (opts) => coerceBigInts(await binding.overheadTrim(opts)),
  hotspots: async (opts) => coerceBigInts(await binding.hotspots(opts)),
  compare: async (opts) => coerceBigInts(await binding.compare(opts)),
  search: async (opts) => coerceBigInts(await binding.search(opts)),
  exportLedger: async (opts) => coerceBigInts(await binding.exportLedger(opts)),
  exportStamps: async (opts) => coerceBigInts(await binding.exportStamps(opts)),
  BurnErrorCode: binding.BurnErrorCode,
  OverheadFileKind: binding.OverheadFileKind,
  HotspotsGroupBy: binding.HotspotsGroupBy,
};
