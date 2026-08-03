#!/usr/bin/env node
import { readFile, writeFile } from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const OUTS = [
  path.resolve(__dirname, '..', 'crates', 'relayburn-sdk', 'data', 'models.dev.json'),
];
const PRIMARY_MODEL_IDS = path.resolve(
  __dirname,
  '..',
  'crates',
  'relayburn-sdk',
  'data',
  'primary-model-ids.json',
);
const PRIMARY_PRICING_PROVIDERS = [
  'anthropic',
  'openai',
  'google',
  'google-vertex',
  'xai',
];

function primaryModelIds(snapshot) {
  return PRIMARY_PRICING_PROVIDERS.flatMap((provider) =>
    Object.keys(snapshot[provider]?.models ?? {}),
  );
}

const res = await fetch('https://models.dev/api.json');
if (!res.ok) {
  console.error(`fetch failed: ${res.status} ${res.statusText}`);
  process.exit(1);
}
const body = await res.text();
const incoming = JSON.parse(body);
const outgoing = JSON.parse(await readFile(OUTS[0], 'utf8'));
// Retain the full ownership catalog, including burn-defined aliases such as
// codex-auto-review, then add every outgoing/incoming first-party snapshot ID.
const retained = JSON.parse(await readFile(PRIMARY_MODEL_IDS, 'utf8'));
const primaryIds = [
  ...new Set([
    ...retained,
    ...primaryModelIds(outgoing),
    ...primaryModelIds(incoming),
  ]),
].sort();

await writeFile(PRIMARY_MODEL_IDS, `${JSON.stringify(primaryIds, null, 2)}\n`, 'utf8');
console.log(`wrote ${PRIMARY_MODEL_IDS} (${primaryIds.length} primary model ids)`);
for (const out of OUTS) {
  await writeFile(out, body, 'utf8');
  console.log(`wrote ${out} (${body.length} bytes)`);
}
