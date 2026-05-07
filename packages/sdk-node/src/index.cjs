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
  ingest: async (opts) => binding.ingest(opts),
  summary: async (opts) => binding.summary(opts),
  sessionCost: async (opts) => binding.sessionCost(opts),
  overhead: async (opts) => binding.overhead(opts),
  overheadTrim: async (opts) => binding.overheadTrim(opts),
  hotspots: async (opts) => binding.hotspots(opts),
  compare: async (opts) => binding.compare(opts),
  search: async (opts) => binding.search(opts),
  exportLedger: async (opts) => binding.exportLedger(opts),
  exportStamps: async (opts) => binding.exportStamps(opts),
  BurnErrorCode: binding.BurnErrorCode,
  OverheadFileKind: binding.OverheadFileKind,
  HotspotsGroupBy: binding.HotspotsGroupBy,
};
