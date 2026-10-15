// The test runner loads scripts/test-seal.mjs, so no test can open the
// user's real ai-hist store, burn ledger or harness data: every default the
// native SDK resolves lands beneath a tripwire file and fails loudly.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { statSync } from 'node:fs';
import { homedir } from 'node:os';
import { basename } from 'node:path';
import { loadNapiSdk } from './helpers/napi.js';

const LOUD = /Not a directory|no-home-in-tests/;

test('HOME is the sealed tripwire file', () => {
  assert.equal(basename(homedir()), 'no-home-in-tests', 'run tests through `pnpm run test`');
  assert.ok(statSync(homedir()).isFile());
  for (const key of ['AI_HIST_DB', 'RELAYBURN_HOME', 'XDG_DATA_HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR']) {
    assert.equal(process.env[key], undefined, key);
  }
});

test('verbs called without explicit homes fail at the tripwire', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;
  await assert.rejects(sdk.ingest({}), LOUD);
  await assert.rejects(sdk.summary({}), LOUD);
});
