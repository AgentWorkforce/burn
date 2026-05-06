// CommonJS variant of the umbrella facade. Required for tools that resolve
// `require('@relayburn/sdk')` through CJS — the ESM `src/index.js` is the
// canonical entry point but pnpm / Node falls back to this when a consumer's
// package is `"type": "commonjs"` and the `exports.require` map is honored.

'use strict';

const binding = require('./binding.js');

module.exports = {
  Ledger: binding.Ledger,
  ingest: (opts) => binding.ingest(opts),
  summary: (opts) => binding.summary(opts),
  sessionCost: (opts) => binding.sessionCost(opts),
  overhead: (opts) => binding.overhead(opts),
  overheadTrim: (opts) => binding.overheadTrim(opts),
  hotspots: (opts) => binding.hotspots(opts),
  compare: (opts) => binding.compare(opts),
};
