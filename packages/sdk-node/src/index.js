// Thin ESM facade over the napi-rs binding. The verbs here re-export the
// matching `#[napi]` exports from the platform package resolved by
// `./binding.cjs`, with two compatibility adjustments carried forward from
// the 1.x SDK contract:
//
//   1. The sync `#[napi]` verbs (`summary`, `sessionCost`, `overhead`,
//      `overheadTrim`, `hotspots`, `compare`) are re-exported as `async`
//      functions so callers receive `Promise<T>` (matching the 1.x
//      `Promise<...>` return shape). Awaiting an `async` wrapper around a
//      sync return is free and preserves the typed `e.code` thrown by the
//      binding (`BurnErrorCode.Sdk` / `Io` / `InvalidArgument`).
//   2. `Ledger.open()` is a JS-side wrapper around the binding's
//      `ledgerOpen()` smoke verb. The 1.x `Ledger` class only exposes a
//      static `open(opts)` that returns a placeholder instance, so we
//      keep the same shape here without adding a stateful `#[napi]` class
//      (which would force `Mutex<LedgerHandle>` plumbing for no benefit).
//
// All query / compute logic lives in the Rust SDK (`crates/relayburn-sdk`);
// the binding crate (`crates/relayburn-sdk-node`) wraps it for napi-rs.

import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const binding = require('./binding.cjs');

// napi-rs serializes Rust `u64` / `i64` as JS `BigInt`, while 1.x emitted
// plain `Number` for the same fields. To keep common caller expectations
// intact (e.g. `result.turnCount === 0`, not `=== 0n`), downcast every
// `BigInt` in a verb's return value to `Number` when it fits in
// `[Number.MIN_SAFE_INTEGER, Number.MAX_SAFE_INTEGER]`. Values outside that
// range are left as `BigInt`; leaking a `BigInt` is safer than silently
// rounding a very large ledger.
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
    // Skip class instances we don't own (Date, Map, Set, Buffer, …) —
    // walking their guts would be both wasteful and risky. Plain objects
    // produced by napi-rs serde have a null or Object prototype.
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

/**
 * Stateful ledger handle. The 1.x SDK only exposed the static `open(opts)`
 * constructor; instance methods are reserved for a future PR. Keep that shape
 * and stash the resolved home for introspection.
 */
export class Ledger {
  constructor(home) {
    /** @type {string} */
    this.home = home;
  }
  /**
   * Open and validate a ledger at `opts.home` (or `RELAYBURN_HOME`).
   * Returns a `Promise<Ledger>` to mirror the 1.x async signature even
   * though the underlying `ledgerOpen` binding is synchronous.
   *
   * @param {{ home?: string, contentHome?: string }} [opts]
   * @returns {Promise<Ledger>}
   */
  static async open(opts) {
    const home = binding.ledgerOpen(opts);
    return new Ledger(home);
  }
}

export async function ingest(opts) {
  return coerceBigInts(await binding.ingest(opts));
}

export async function summary(opts) {
  return coerceBigInts(await binding.summary(opts));
}

export async function sessionCost(opts) {
  return coerceBigInts(await binding.sessionCost(opts));
}

export async function overhead(opts) {
  return coerceBigInts(await binding.overhead(opts));
}

export async function overheadTrim(opts) {
  return coerceBigInts(await binding.overheadTrim(opts));
}

export async function hotspots(opts) {
  return coerceBigInts(await binding.hotspots(opts));
}

export async function compare(opts) {
  return coerceBigInts(await binding.compare(opts));
}

export function computeCompareExcluded(summary, minimum) {
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

// 2.x extensions — exposed by the Rust SDK but not declared in
// the 1.x SDK surface. These let embedders reach the FTS5 search index and
// JSONL export iterators without dropping into the binding directly.
export async function search(opts) {
  return coerceBigInts(await binding.search(normalizeSearchOptions(opts)));
}

export async function exportLedger(opts) {
  return coerceBigInts(await binding.exportLedger(opts));
}

export async function exportStamps(opts) {
  return coerceBigInts(await binding.exportStamps(opts));
}

// Re-exported enums from the Rust binding. These come across as plain
// string-valued objects (`{ Sdk: 'BURN_SDK', ... }`) and let JS callers
// branch on `e.code === BurnErrorCode.Sdk` without stringly-typed
// literals. `OverheadFileKind` and `HotspotsGroupBy` are likewise the
// canonical wire values for the matching option-struct fields.
export const BurnErrorCode = binding.BurnErrorCode;
export const OverheadFileKind = binding.OverheadFileKind;
export const HotspotsGroupBy = binding.HotspotsGroupBy;
