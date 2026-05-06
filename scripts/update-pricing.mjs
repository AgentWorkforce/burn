#!/usr/bin/env node
import { writeFile } from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
// Two vendored copies are kept in lockstep: the TS analyze package reads from
// the packages/ copy, and the Rust SDK crate `include_str!`s the crates/
// copy (required for `cargo package` to bundle it cleanly).
const OUTS = [
  path.resolve(__dirname, '..', 'packages', 'analyze', 'pricing', 'models.dev.json'),
  path.resolve(__dirname, '..', 'crates', 'relayburn-sdk', 'data', 'models.dev.json'),
];

const res = await fetch('https://models.dev/api.json');
if (!res.ok) {
  console.error(`fetch failed: ${res.status} ${res.statusText}`);
  process.exit(1);
}
const body = await res.text();
for (const out of OUTS) {
  await writeFile(out, body, 'utf8');
  console.log(`wrote ${out} (${body.length} bytes)`);
}
