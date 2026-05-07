// Thin ESM facade over the napi-rs binding. The verbs here re-export the
// matching `#[napi]` exports from the platform package resolved by
// `./binding.cjs`, with two adjustments to match the TS 1.x contract at
// `packages/sdk/index.d.ts`:
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

// napi-rs serializes Rust `u64` / `i64` as JS `BigInt`, but the TS 1.x
// `@relayburn/sdk` shape (mirrored in `src/index.d.ts`) emits plain
// `Number` for the same fields. To keep the conformance gate's
// `deepStrictEqual` checks honest — and to match the runtime shape that
// 1.x callers expect (e.g. `result.turnCount === 0`, not `=== 0n`) — we
// downcast every `BigInt` in a verb's return value to `Number` when it
// fits in `[Number.MIN_SAFE_INTEGER, Number.MAX_SAFE_INTEGER]`. Values
// outside that range are left as `BigInt`: realistic burn ledgers won't
// hit 2^53 tokens, but if one ever does, leaking a `BigInt` that crashes
// a `===` check is strictly safer than silently rounding to the nearest
// 1024. The TS shape declares `number | bigint` everywhere this matters
// so the type stays sound either way.
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

/**
 * Stateful ledger handle. Mirrors the TS 1.x `Ledger` class shape from
 * `packages/sdk/index.d.ts`. The 1.x version only exposes the static
 * `open(opts)` constructor — instance methods are reserved for a future
 * PR — so we replicate that surface and stash the resolved home for
 * introspection.
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

// 2.x extensions — exposed by the Rust SDK but not declared in
// `packages/sdk/index.d.ts` (the 1.x TS surface). Per the SDK shape rule,
// pre-1.0 widening is allowed; these are surfaced here so embedders can
// reach the FTS5 search index and the JSONL export iterators without
// dropping into the binding directly.
export async function search(opts) {
  return coerceBigInts(await binding.search(opts));
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
