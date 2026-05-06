// Conformance test: TS @relayburn/sdk@1.x vs napi-rs @relayburn/sdk@2.0.0-pre.
//
// For each of the 7 verbs (`ingest`, `summary`, `sessionCost`, `overhead`,
// `overheadTrim`, `hotspots`, `compare`), run both implementations against
// the existing fixture ledger and assert `deepStrictEqual` on the return
// value. This is the gate that says "the Rust port is behavior-equivalent
// to the TS port" — it's the test #247 cites as the acceptance criterion.
//
// **Status (2026-05-06): scaffolded but skipped.** The napi-rs bindings
// land in #247-a (parallel agent worktree). Until that crate is published
// and the umbrella's `src/binding.cjs` resolves a real native package, the
// `loadNapiSdk()` helper below throws and the suite skips. Flip the
// `RELAYBURN_SDK_NAPI_BUILT=1` env var (CI sets this once #247-a's bindings
// produce a valid `*.node` artifact) to enable the comparison.
//
// What's stubbed and why:
//   - The local `@relayburn/sdk` npm package in this directory is the 2.x
//     umbrella; its facade resolves the binding lazily on import. So
//     `await import('@relayburn/sdk')` succeeds, but the first verb call
//     throws "binding not found" until #247-a ships. We catch that
//     specific error and skip rather than fail.
//   - We fix `RELAYBURN_HOME` to a tmp dir for both runs so they share
//     state. The TS 1.x `@relayburn/sdk` is loaded via a relative file:
//     reference (`packages/sdk`) to avoid name collision with the 2.x
//     umbrella in this package's `node_modules`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, cpSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '../../..');

// Fixture ledger location. CI seeds `tests/fixtures/ledger/` from the
// reader corpus during the `prepare-fixture-ledger` step. When
// `RELAYBURN_SDK_NAPI_BUILT=1` is set this directory MUST exist —
// `ensureFixtureLedger()` below throws if it's missing so the gate can't
// silently pass on empty homes.
const FIXTURE_LEDGER_DIR = join(REPO_ROOT, 'tests', 'fixtures', 'ledger');

const NAPI_READY = process.env.RELAYBURN_SDK_NAPI_BUILT === '1';

async function loadTsSdk() {
  // The 1.x SDK lives at `packages/sdk` — load it via a fully-resolved
  // relative path so we don't accidentally resolve back into the 2.x
  // umbrella we're testing from.
  const tsSdkPath = join(REPO_ROOT, 'packages', 'sdk', 'index.js');
  return import(tsSdkPath);
}

async function loadNapiSdk() {
  // Resolve the in-tree umbrella facade. Throws if the binding is missing —
  // the per-test guard below converts that to a skip so the suite stays
  // green while #247-a is in flight.
  const napiSdkPath = join(__dirname, '..', 'src', 'index.js');
  return import(napiSdkPath);
}

function bindingMissing(err) {
  return /native binding not found/i.test(String(err && err.message));
}

// When the gate is on (`RELAYBURN_SDK_NAPI_BUILT=1`) the fixture ledger MUST
// exist — otherwise both implementations would compare against empty homes
// and the deep-equality checks would tautologically pass. Throwing here turns
// "fixture missing" into a loud failure instead of a silent green.
//
// The fixture lives at `tests/fixtures/ledger/` and should mirror the on-disk
// shape of `~/.relayburn/` (typically a `ledger.jsonl` plus `content/`
// sidecar). CI's `prepare-fixture-ledger` step seeds it from the reader
// corpus before flipping the gate.
function ensureFixtureLedger() {
  if (!existsSync(FIXTURE_LEDGER_DIR)) {
    throw new Error(
      `conformance fixture ledger missing at ${FIXTURE_LEDGER_DIR}. ` +
        `Seed it (e.g. cp -R ~/.relayburn tests/fixtures/ledger) before ` +
        `running with RELAYBURN_SDK_NAPI_BUILT=1, or unset the env var to ` +
        `skip the conformance suite.`,
    );
  }
}

function makeHome() {
  const home = mkdtempSync(join(tmpdir(), 'relayburn-conformance-'));
  cpSync(FIXTURE_LEDGER_DIR, home, { recursive: true });
  return home;
}

async function callBoth(verb, opts) {
  ensureFixtureLedger();
  const ts = await loadTsSdk();
  const napi = await loadNapiSdk();
  const tsHome = makeHome();
  const napiHome = makeHome();
  try {
    const tsResult = await ts[verb]({ ...opts, ledgerHome: tsHome });
    let napiResult;
    try {
      napiResult = await napi[verb]({ ...opts, ledgerHome: napiHome });
    } catch (err) {
      if (bindingMissing(err)) return { skipped: true };
      throw err;
    }
    return { tsResult, napiResult };
  } finally {
    rmSync(tsHome, { recursive: true, force: true });
    rmSync(napiHome, { recursive: true, force: true });
  }
}

const VERBS = [
  { name: 'summary', opts: {} },
  { name: 'sessionCost', opts: { session: 'fixture-session-1' } },
  { name: 'overhead', opts: { project: REPO_ROOT } },
  { name: 'overheadTrim', opts: { project: REPO_ROOT, includeDiff: false } },
  { name: 'hotspots', opts: {} },
  // compare() requires >=2 models; pick a stable pair that may or may not
  // appear in the fixture — both implementations see the same input so
  // missing models surface as identical empty/no-data rows on each side.
  // The deep-equality check still catches drift in cell shape, fidelity
  // accounting, and totals.
  {
    name: 'compare',
    opts: {
      models: ['claude-sonnet-4-5', 'claude-opus-4-7'],
      minFidelity: 'partial',
    },
  },
  // ingest is exercised separately because both impls mutate the ledger;
  // we run it last in its own block.
];

for (const { name, opts } of VERBS) {
  test(`conformance: ${name}() matches TS 1.x`, async (t) => {
    if (!NAPI_READY) {
      t.skip('napi-rs binding not built — set RELAYBURN_SDK_NAPI_BUILT=1 once #247-a lands');
      return;
    }
    const out = await callBoth(name, opts);
    if (out.skipped) {
      t.skip('napi-rs binding load failed — #247-a binding artifact missing');
      return;
    }
    assert.deepStrictEqual(out.napiResult, out.tsResult);
  });
}

test('conformance: ingest() matches TS 1.x', async (t) => {
  if (!NAPI_READY) {
    t.skip('napi-rs binding not built — set RELAYBURN_SDK_NAPI_BUILT=1 once #247-a lands');
    return;
  }
  // ingest mutates the ledger, so we deep-equal the *returned report*.
  // Once the napi binding is wired up we may also want to compare a
  // follow-up summary() across both homes to confirm the two
  // implementations wrote the same rows; that's tracked separately.
  const out = await callBoth('ingest', {});
  if (out.skipped) {
    t.skip('napi-rs binding load failed — #247-a binding artifact missing');
    return;
  }
  assert.deepStrictEqual(out.napiResult, out.tsResult);
});
