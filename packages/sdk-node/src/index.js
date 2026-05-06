// Thin ESM facade over the napi-rs binding. No behavior — every function
// here re-exports the matching `#[napi]` export from the platform package
// resolved by `./binding.cjs`.
//
// All query / compute logic lives in the Rust SDK (`crates/relayburn-sdk`);
// the binding crate (`crates/relayburn-sdk-node`) wraps it for napi-rs.
// This file exists so the published TS surface stays identical to the
// 1.x SDK (`packages/sdk/index.js`) — same import names, same option shapes,
// same return types — while the runtime is Rust.

import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const binding = require('./binding.cjs');

export const Ledger = binding.Ledger;

export function ingest(opts) {
  return binding.ingest(opts);
}

export function summary(opts) {
  return binding.summary(opts);
}

export function sessionCost(opts) {
  return binding.sessionCost(opts);
}

export function overhead(opts) {
  return binding.overhead(opts);
}

export function overheadTrim(opts) {
  return binding.overheadTrim(opts);
}

export function hotspots(opts) {
  return binding.hotspots(opts);
}

export function compare(opts) {
  return binding.compare(opts);
}
