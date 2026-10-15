// The test runner loads scripts/test-seal.mjs, so no test can open the
// user's real ai-hist store, burn ledger or harness data: every default the
// native SDK resolves lands beneath a tripwire file and fails loudly.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { statSync } from 'node:fs';
import { homedir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { relocatesAStore } from '../../../scripts/test-seal.mjs';

const SEAL = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', 'scripts', 'test-seal.mjs');
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

test('store-relocating names are matched case-insensitively', () => {
  for (const key of ['RELAYBURN_HOME', 'Relayburn_Home', 'ai_hist_db', 'Codex_Home', 'xdg_data_home']) {
    assert.ok(relocatesAStore(key), key);
  }
  for (const key of ['PATH', 'RELAYBURN_CONTENT_STORE', 'relayburn_sdk_napi_built']) {
    assert.ok(!relocatesAStore(key), key);
  }
  const child = spawnSync(
    process.execPath,
    ['--import', SEAL, '-e', 'console.log(JSON.stringify(Object.keys(process.env)))'],
    { env: { ...process.env, Relayburn_Home: '/real', ai_hist_db: '/real.db' }, encoding: 'utf8' },
  );
  assert.equal(child.status, 0, child.stderr);
  const survivors = JSON.parse(child.stdout).filter((key) => relocatesAStore(key));
  assert.deepEqual(survivors, []);
});
