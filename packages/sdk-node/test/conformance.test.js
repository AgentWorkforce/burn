// Conformance test: TS @relayburn/sdk@1.x vs napi-rs @relayburn/sdk@2.0.0-pre.
//
// For each of the 7 verbs (`ingest`, `summary`, `sessionCost`, `overhead`,
// `overheadTrim`, `hotspots`, `compare`), run both implementations against
// the same seeded fixture ledger and assert `deepStrictEqual` on the return
// value. This is the gate that says "the Rust port is behavior-equivalent
// to the TS port" — it's the test #247 cites as the acceptance criterion.
//
// **Fixture strategy.** Rather than commit a binary-ish ledger snapshot to
// `tests/fixtures/ledger/`, we seed a fresh ledger on every test run by
// invoking the deterministic builder at
// `tests/fixtures/cli-golden/scripts/build-ledger.mjs`. That script is the
// hand-curated source of truth for the cli-golden suite — it exercises all
// three readers (claude-code / codex / opencode), multi-session shapes,
// edits + tool calls + stamps + relationships — and is byte-deterministic.
// We `cpSync` its output into two tmp homes (one for each impl) so reads
// happen against identical state. This keeps conformance self-contained
// (no committed `tests/fixtures/ledger/` to drift silently) and keeps the
// fixture in lock-step with the cli-golden tests that already maintain it.
//
// **Ingest is read-only here.** `ingest` is one of the 7 verbs we conform,
// and ingesting via the live `ingestAll()` would scan the runner's real
// `~/.claude/projects/`, `~/.codex/sessions/`, etc., making the test
// non-deterministic. We isolate `HOME` to an empty tmp dir for both impls,
// so `ingest` returns the trivial `{ scannedSessions: 0, ... }` report on
// both sides. Deep ingest conformance against a corpus is tracked as a
// follow-up — see the comment on the ingest test below.
//
// **Loader contract.** The local `@relayburn/sdk` (this package) is the 2.x
// umbrella; its facade resolves the binding lazily on import. So
// `await import('./src/index.js')` succeeds but the first verb call throws
// if the binding is missing. We catch that specific error and skip rather
// than fail.
//
// Set `RELAYBURN_SDK_NAPI_BUILT=1` to actually run the comparison;
// `napi-build.yml` flips the gate after `pnpm run build:napi` produces a
// resolvable `.node`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, cpSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '../../..');

// Seeder script — see header. The script reads `RELAYBURN_HOME` and writes
// `ledger.jsonl` + `ledger.idx` + `ledger.content.idx` into it.
const SEEDER_SCRIPT = join(
  REPO_ROOT,
  'tests',
  'fixtures',
  'cli-golden',
  'scripts',
  'build-ledger.mjs',
);

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
  // green when the build artifact isn't present (e.g. `npm test` without
  // `pnpm run build:napi` first).
  const napiSdkPath = join(__dirname, '..', 'src', 'index.js');
  return import(napiSdkPath);
}

function bindingMissing(err) {
  return /native binding not found/i.test(String(err && err.message));
}

// Module-scoped seed dir, populated by `seedFixture()` on first use and
// reused across tests. Cleaned up by the `after` hook below.
let SEED_HOME = null;

function seedFixture() {
  if (SEED_HOME !== null) return SEED_HOME;
  const dir = mkdtempSync(join(tmpdir(), 'relayburn-conformance-seed-'));
  // Spawn the deterministic builder in a fresh process so its imports
  // resolve from the workspace root regardless of our `cwd`. The script
  // uses `@relayburn/ledger` etc., which need the workspace built (see
  // `pnpm run build` in CI before this test runs).
  const result = spawnSync(process.execPath, [SEEDER_SCRIPT], {
    env: { ...process.env, RELAYBURN_HOME: dir },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  if (result.status !== 0) {
    const stderr = result.stderr?.toString() ?? '';
    const stdout = result.stdout?.toString() ?? '';
    throw new Error(
      `conformance fixture seeder failed (exit ${result.status}):\n` +
        `--- stderr ---\n${stderr}\n--- stdout ---\n${stdout}`,
    );
  }
  SEED_HOME = dir;
  return dir;
}

test.after(() => {
  if (SEED_HOME) rmSync(SEED_HOME, { recursive: true, force: true });
});

// Empty fake-HOME tree. `ingest` discovers session stores under
// `~/.claude/projects/`, `~/.codex/sessions/`, and
// `~/.local/share/opencode/storage/` (see `packages/ingest/src/ingest.ts`).
// We seed empty dirs so both impls return the trivial empty report instead
// of scanning the real user's session logs.
function makeEmptyHome() {
  const home = mkdtempSync(join(tmpdir(), 'relayburn-conformance-home-'));
  mkdirSync(join(home, '.claude', 'projects'), { recursive: true });
  mkdirSync(join(home, '.codex', 'sessions'), { recursive: true });
  mkdirSync(join(home, '.local', 'share', 'opencode', 'storage'), {
    recursive: true,
  });
  return home;
}

function makeLedgerHome() {
  const seed = seedFixture();
  const home = mkdtempSync(join(tmpdir(), 'relayburn-conformance-ledger-'));
  cpSync(seed, home, { recursive: true });
  return home;
}

// Run both impls' `verb(opts)` against fresh-but-identical seeded ledger
// homes and return their results. Caller asserts `deepStrictEqual` on the
// pair. If the napi binding isn't loadable yet, returns `{ skipped: true }`
// so callers can skip the test rather than fail.
async function callBoth(verb, opts) {
  const ts = await loadTsSdk();
  const napi = await loadNapiSdk();
  const tsHome = makeLedgerHome();
  const napiHome = makeLedgerHome();
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
  // `11111111-...` is the Claude session A id seeded by build-ledger.mjs;
  // it has 4 turns of edits + tests + bash so sessionCost has real numbers
  // to diff against. See `tests/fixtures/cli-golden/scripts/build-ledger.mjs`.
  { name: 'sessionCost', opts: { session: '11111111-1111-1111-1111-111111111111' } },
  // overhead/overheadTrim need a project path that resolves; the seeded
  // ledger uses `/tmp/golden-project` as its project, but that won't have
  // settings on disk. Both impls see the same missing-project state, so the
  // returned shape (typically empty rows + zero counts) still deep-equals.
  { name: 'overhead', opts: { project: '/tmp/golden-project' } },
  { name: 'overheadTrim', opts: { project: '/tmp/golden-project', includeDiff: false } },
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
];

for (const { name, opts } of VERBS) {
  test(`conformance: ${name}() matches TS 1.x`, async (t) => {
    if (!NAPI_READY) {
      t.skip('napi-rs binding not built — set RELAYBURN_SDK_NAPI_BUILT=1');
      return;
    }
    const out = await callBoth(name, opts);
    if (out.skipped) {
      t.skip('napi-rs binding load failed — build artifact missing');
      return;
    }
    assert.deepStrictEqual(out.napiResult, out.tsResult);
  });
}

test('conformance: ingest() matches TS 1.x', async (t) => {
  if (!NAPI_READY) {
    t.skip('napi-rs binding not built — set RELAYBURN_SDK_NAPI_BUILT=1');
    return;
  }
  // Both impls scan session stores via `homedir()` (no env override). To
  // keep this deterministic we point HOME at an empty tmp tree so ingest
  // returns the trivial `{ scannedSessions: 0, ingestedSessions: 0,
  // appendedTurns: 0 }` report on both sides. Deep-corpus ingest
  // conformance — point both impls at the same `~/.claude/projects/`
  // tree of fixture session logs and compare the resulting ledgers — is
  // tracked as an α follow-up; the seeded ledger here is hand-curated, not
  // ingest output, so it can't double as that test.
  const ts = await loadTsSdk();
  const napi = await loadNapiSdk();
  const fakeHome = makeEmptyHome();
  const tsLedgerHome = makeLedgerHome();
  const napiLedgerHome = makeLedgerHome();
  const prevHome = process.env.HOME;
  const prevUserprofile = process.env.USERPROFILE;
  try {
    process.env.HOME = fakeHome;
    process.env.USERPROFILE = fakeHome; // win32 fallback in node:os.homedir()
    const tsResult = await ts.ingest({ ledgerHome: tsLedgerHome });
    let napiResult;
    try {
      napiResult = await napi.ingest({ ledgerHome: napiLedgerHome });
    } catch (err) {
      if (bindingMissing(err)) {
        t.skip('napi-rs binding load failed — build artifact missing');
        return;
      }
      throw err;
    }
    assert.deepStrictEqual(napiResult, tsResult);
  } finally {
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
    if (prevUserprofile === undefined) delete process.env.USERPROFILE;
    else process.env.USERPROFILE = prevUserprofile;
    rmSync(fakeHome, { recursive: true, force: true });
    rmSync(tsLedgerHome, { recursive: true, force: true });
    rmSync(napiLedgerHome, { recursive: true, force: true });
  }
});
