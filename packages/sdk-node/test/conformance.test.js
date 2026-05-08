// Native SDK smoke tests for the `@relayburn/sdk` 2.x facade.
//
// These tests run the napi-rs facade against the committed cli-golden ledger.
// They are intentionally shape-level checks now that the old TypeScript SDK
// package has been removed from the workspace. Set RELAYBURN_SDK_NAPI_BUILT=1
// after `pnpm run build:napi` to execute them; without a native binding they
// skip cleanly so package-level JS tests still work on a fresh checkout.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, cpSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '../../..');
const FIXTURE_LEDGER = join(REPO_ROOT, 'tests', 'fixtures', 'cli-golden', 'ledger');
const NAPI_READY = process.env.RELAYBURN_SDK_NAPI_BUILT === '1';

function bindingMissing(err) {
  return /native binding not found/i.test(String(err && err.message));
}

async function loadNapiSdk(t) {
  if (!NAPI_READY) {
    t.skip('napi-rs binding not built; set RELAYBURN_SDK_NAPI_BUILT=1');
    return null;
  }
  try {
    return await import(join(__dirname, '..', 'src', 'index.js'));
  } catch (err) {
    if (bindingMissing(err)) {
      t.skip('napi-rs binding load failed; build artifact missing');
      return null;
    }
    throw err;
  }
}

function makeLedgerHome() {
  const home = mkdtempSync(join(tmpdir(), 'relayburn-sdk-ledger-'));
  cpSync(FIXTURE_LEDGER, home, { recursive: true });
  return home;
}

function makeEmptyHome() {
  const home = mkdtempSync(join(tmpdir(), 'relayburn-sdk-home-'));
  mkdirSync(join(home, '.claude', 'projects'), { recursive: true });
  mkdirSync(join(home, '.codex', 'sessions'), { recursive: true });
  mkdirSync(join(home, '.local', 'share', 'opencode', 'storage'), {
    recursive: true,
  });
  return home;
}

test('sdk facade exposes the expected verb set', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  for (const name of [
    'Ledger',
    'ingest',
    'summary',
    'sessionCost',
    'overhead',
    'overheadTrim',
    'hotspots',
    'compare',
    'computeCompareExcluded',
    'search',
    'exportLedger',
    'exportStamps',
  ]) {
    assert.equal(typeof sdk[name], 'function', `${name} should be exported`);
  }
});

test('read verbs return stable shapes against the fixture ledger', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const ledgerHome = makeLedgerHome();
  try {
    const summary = await sdk.summary({ ledgerHome });
    assert.equal(typeof summary.totalCost, 'number');
    assert.ok(Array.isArray(summary.byModel));
    assert.ok(Array.isArray(summary.byTool));

    const session = await sdk.sessionCost({
      ledgerHome,
      session: '11111111-1111-1111-1111-111111111111',
    });
    assert.equal(session.sessionId, '11111111-1111-1111-1111-111111111111');
    assert.equal(typeof session.totalUSD, 'number');

    const overhead = await sdk.overhead({ ledgerHome, project: '/tmp/golden-project' });
    assert.equal(overhead.project, '/tmp/golden-project');
    assert.ok(Array.isArray(overhead.files));

    const trim = await sdk.overheadTrim({
      ledgerHome,
      project: '/tmp/golden-project',
      includeDiff: false,
    });
    assert.equal(trim.project, '/tmp/golden-project');
    assert.ok(Array.isArray(trim.recommendations));

    const hotspots = await sdk.hotspots({ ledgerHome });
    assert.equal(typeof hotspots.kind, 'string');

    const compare = await sdk.compare({
      ledgerHome,
      models: ['claude-sonnet-4-5', 'claude-opus-4-7'],
      minFidelity: 'partial',
    });
    assert.ok(Array.isArray(compare.cells));
    assert.equal(compare.fidelity.minimum, 'partial');
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('2.x extension verbs return stable shapes against the fixture ledger', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const ledgerHome = makeLedgerHome();
  try {
    const search = await sdk.search({ ledgerHome, query: 'golden', limit: 5 });
    assert.equal(search.query, 'golden');
    assert.ok(Array.isArray(search.hits));

    const ledgerRows = await sdk.exportLedger({ ledgerHome });
    assert.ok(Array.isArray(ledgerRows));
    assert.ok(ledgerRows.length > 0);

    const stampRows = await sdk.exportStamps({ ledgerHome });
    assert.ok(Array.isArray(stampRows));

    const excluded = sdk.computeCompareExcluded(
      {
        total: 10,
        byClass: {
          full: 1,
          'usage-only': 2,
          'aggregate-only': 3,
          'cost-only': 4,
          partial: 5,
        },
        unknown: 0,
        missingCoverage: {},
      },
      'usage-only',
    );
    assert.deepStrictEqual(excluded, {
      total: 12,
      aggregateOnly: 3,
      costOnly: 4,
      partial: 5,
      usageOnly: 0,
    });
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('ingest scans an isolated empty home', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const fakeHome = makeEmptyHome();
  const ledgerHome = makeLedgerHome();
  const prevHome = process.env.HOME;
  const prevUserprofile = process.env.USERPROFILE;
  try {
    process.env.HOME = fakeHome;
    process.env.USERPROFILE = fakeHome;
    const report = await sdk.ingest({ ledgerHome });
    assert.equal(typeof report.scannedSessions, 'number');
    assert.equal(typeof report.ingestedSessions, 'number');
    assert.equal(typeof report.appendedTurns, 'number');
  } finally {
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
    if (prevUserprofile === undefined) delete process.env.USERPROFILE;
    else process.env.USERPROFILE = prevUserprofile;
    rmSync(fakeHome, { recursive: true, force: true });
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});
