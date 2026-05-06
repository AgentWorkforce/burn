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
import { mkdtempSync, rmSync, cpSync, existsSync, readFileSync, statSync } from 'node:fs';
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
// exist AND be well-formed enough to actually exercise the verbs — otherwise
// both implementations would compare against empty/garbage homes and the
// deep-equality checks would tautologically pass on noise (both sides see the
// same nothing). Throwing here turns "fixture missing or malformed" into a
// loud failure instead of a silent green.
//
// The fixture lives at `tests/fixtures/ledger/` and mirrors the on-disk shape
// of `~/.relayburn/`: a canonical `ledger.jsonl` (see
// `packages/ledger/src/paths.ts::ledgerPath`) plus a `content/` sidecar. CI's
// `prepare-fixture-ledger` step seeds it from the reader corpus before
// flipping the gate.
//
// Preconditions checked:
//   1. The fixture directory exists.
//   2. `ledger.jsonl` exists and is non-empty.
//   3. The first line of `ledger.jsonl` parses as JSON and matches the
//      `LedgerLine` shape (`v: 1` + `kind: <known>`) defined in
//      `packages/ledger/src/schema.ts`. This catches the easy "I copied the
//      wrong thing" failure mode (e.g. a stray text file) without doing a
//      full schema sweep.
//   4. At least one `kind: 'turn'` line is present anywhere in the file —
//      verbs like `summary` / `hotspots` / `compare` need turns to produce
//      non-trivial output, so a stamp-only fixture would still let the
//      conformance gate pass on empty rows.
const KNOWN_LEDGER_KINDS = new Set([
  'turn',
  'stamp',
  'compaction',
  'relationship',
  'tool_result_event',
  'user_turn',
]);

function fixtureSeedHint() {
  return (
    `Seed it (e.g. cp -R ~/.relayburn tests/fixtures/ledger) before ` +
    `running with RELAYBURN_SDK_NAPI_BUILT=1, or unset the env var to ` +
    `skip the conformance suite.`
  );
}

function ensureFixtureLedger() {
  if (!existsSync(FIXTURE_LEDGER_DIR)) {
    throw new Error(
      `conformance fixture ledger missing at ${FIXTURE_LEDGER_DIR}. ` +
        fixtureSeedHint(),
    );
  }
  const ledgerJsonl = join(FIXTURE_LEDGER_DIR, 'ledger.jsonl');
  if (!existsSync(ledgerJsonl)) {
    throw new Error(
      `conformance fixture ledger malformed: expected ${ledgerJsonl} ` +
        `(canonical ledger filename per packages/ledger/src/paths.ts). ` +
        fixtureSeedHint(),
    );
  }
  if (statSync(ledgerJsonl).size === 0) {
    throw new Error(
      `conformance fixture ledger malformed: ${ledgerJsonl} is empty. ` +
        fixtureSeedHint(),
    );
  }
  // Cheap sanity sweep: confirm the file looks like a JSONL ledger and
  // contains at least one turn line. We read the whole file because real
  // fixtures are small (kilobytes); if that ever stops being true, switch to
  // a streaming scan.
  const lines = readFileSync(ledgerJsonl, 'utf8').split('\n').filter((l) => l.length > 0);
  if (lines.length === 0) {
    throw new Error(
      `conformance fixture ledger malformed: ${ledgerJsonl} has no JSONL ` +
        `lines. ` +
        fixtureSeedHint(),
    );
  }
  let firstLine;
  try {
    firstLine = JSON.parse(lines[0]);
  } catch (err) {
    throw new Error(
      `conformance fixture ledger malformed: ${ledgerJsonl} first line is ` +
        `not valid JSON (${err && err.message}). ` +
        fixtureSeedHint(),
    );
  }
  if (
    !firstLine ||
    typeof firstLine !== 'object' ||
    firstLine.v !== 1 ||
    typeof firstLine.kind !== 'string' ||
    !KNOWN_LEDGER_KINDS.has(firstLine.kind)
  ) {
    throw new Error(
      `conformance fixture ledger malformed: ${ledgerJsonl} first line ` +
        `does not match the LedgerLine shape (expected v:1 + kind:<known>, ` +
        `got ${JSON.stringify({ v: firstLine && firstLine.v, kind: firstLine && firstLine.kind })}). ` +
        fixtureSeedHint(),
    );
  }
  // Require at least one turn so summary/hotspots/compare have something to
  // diff against. A pure stamp/relationship-only fixture would still let
  // both impls return empty rows and pass on noise.
  const hasTurn = lines.some((line) => {
    try {
      const parsed = JSON.parse(line);
      return parsed && parsed.v === 1 && parsed.kind === 'turn';
    } catch {
      return false;
    }
  });
  if (!hasTurn) {
    throw new Error(
      `conformance fixture ledger malformed: ${ledgerJsonl} contains no ` +
        `kind:'turn' lines, so verbs like summary/hotspots/compare would ` +
        `compare empty results on both sides. ` +
        fixtureSeedHint(),
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
