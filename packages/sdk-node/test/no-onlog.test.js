// Regression guard for issue #374. The 2.x Node facade is SQLite-native and
// has no archive-fallback path to surface, so `onLog` was always a no-op at
// the napi boundary. We removed it from the public option types; this test
// fails if it ever sneaks back in (verb-by-verb), so we don't accidentally
// re-document a callback that doesn't fire.
//
// Also asserts that calling each verb with a stray `onLog` property is
// tolerated at runtime — JS allows extra props on options bags, and we don't
// want to break embedders mid-migration from 1.x just because they haven't
// dropped the field yet.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const DTS_PATH = resolve(__dirname, '..', 'src', 'index.d.ts');

test('public option types no longer declare onLog (#374)', () => {
  const dts = readFileSync(DTS_PATH, 'utf8');
  const offending = dts
    .split('\n')
    .map((line, i) => ({ line, n: i + 1 }))
    .filter(({ line }) => /^\s*onLog\??\s*:/.test(line));
  assert.deepStrictEqual(
    offending,
    [],
    `onLog reappeared in src/index.d.ts at:\n` +
      offending.map(({ n, line }) => `  ${n}: ${line.trim()}`).join('\n'),
  );
});

test('verbs tolerate a stray onLog property at runtime', async (t) => {
  if (process.env.RELAYBURN_SDK_NAPI_BUILT !== '1') {
    t.skip('napi-rs binding not built — set RELAYBURN_SDK_NAPI_BUILT=1');
    return;
  }
  let sdk;
  try {
    sdk = await import(join(__dirname, '..', 'src', 'index.js'));
  } catch (err) {
    if (/native binding not found/i.test(String(err && err.message))) {
      t.skip('napi-rs binding load failed — build artifact missing');
      return;
    }
    throw err;
  }
  // Cast through any-shape to bypass the now-stricter option types: the
  // contract under test is the runtime forgiveness, not the TS shape.
  const stray = { onLog: () => {} };
  await assert.doesNotReject(() => sdk.summary(stray));
  await assert.doesNotReject(() => sdk.sessionCost(stray));
  await assert.doesNotReject(() => sdk.hotspots(stray));
});
