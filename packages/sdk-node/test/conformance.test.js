// Native SDK smoke tests for the `@relayburn/sdk` 2.x facade.
//
// These tests run the napi-rs facade against the committed cli-golden ledger.
// They are intentionally shape-level checks now that the old TypeScript SDK
// package has been removed from the workspace. Set RELAYBURN_SDK_NAPI_BUILT=1
// after `pnpm run build:napi` to execute them. A fresh local checkout skips
// cleanly, while CI fails if the gate or native binding is missing.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  mkdtempSync,
  rmSync,
  cpSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadNapiSdk } from './helpers/napi.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '../../..');
const FIXTURE_LEDGER = join(REPO_ROOT, 'tests', 'fixtures', 'cli-golden', 'ledger');

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

// Config-focused freshness tests must not inherit a caller's env override.
// Top-level node:test cases in this file run sequentially; restore it even if
// an assertion or native call fails.
function clearStaleThresholdEnv(t) {
  const previous = process.env.RELAYBURN_STALE_AFTER_HOURS;
  delete process.env.RELAYBURN_STALE_AFTER_HOURS;
  t.after(() => {
    if (previous === undefined) delete process.env.RELAYBURN_STALE_AFTER_HOURS;
    else process.env.RELAYBURN_STALE_AFTER_HOURS = previous;
  });
}

test('sdk facade exposes the expected verb set', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  for (const name of [
    'Ledger',
    'ingest',
    'summary',
    'ledgerFreshness',
    'sessionCost',
    'measureSession',
    'fingerprint',
    'overhead',
    'overheadTrim',
    'hotspots',
    'compare',
    'writePendingStamp',
    'writeStamp',
    'computeCompareExcluded',
    'search',
    'exportLedger',
    'exportStamps',
    'turnSpanTree',
    'sessionSpanTrees',
    'flowGraph',
    'contextDelta',
  ]) {
    assert.equal(typeof sdk[name], 'function', `${name} should be exported`);
  }
});

test('measureSession reports one explicit transcript without a ledger', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const result = await sdk.measureSession({
    harness: 'codex',
    inputPath: join(REPO_ROOT, 'tests', 'fixtures', 'codex', 'simple-turn.jsonl'),
  });
  assert.equal(result.schema, 'burn.session-metrics.v1');
  assert.equal(result.sessionId, 'sess_simple_1');
  assert.equal(result.turnCount, 1);
  assert.equal(result.usage.inputTokens, 600);
  assert.equal(result.usage.cacheReadTokens, 400);
  assert.equal(result.usage.outputTokens, 120);
  assert.equal(result.usage.reasoningTokens, 30);
  assert.equal(result.models[0].provider, 'openai');
});

test('read verbs return stable shapes against the fixture ledger', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const ledgerHome = makeLedgerHome();
  try {
    const freshness = await sdk.ledgerFreshness({ ledgerHome });
    assert.equal(typeof freshness.stale, 'boolean');
    assert.ok(freshness.staleAfterMs === null || typeof freshness.staleAfterMs === 'number');
    assert.ok(
      freshness.lastWriteAtMs === undefined || typeof freshness.lastWriteAtMs === 'number',
    );
    const summary = await sdk.summary({ ledgerHome });
    assert.equal(typeof summary.totalCost, 'number');
    assert.ok(Array.isArray(summary.byModel));
    assert.ok(Array.isArray(summary.byTool));

    const taggedSummary = await sdk.summary({
      ledgerHome,
      tags: { workflowId: 'wf-golden' },
      groupByTag: 'workflowId',
    });
    assert.equal(taggedSummary.turnCount, 3);
    assert.equal(taggedSummary.byTag[0].tag, 'workflowId');
    assert.equal(taggedSummary.byTag[0].value, 'wf-golden');

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

    assert.equal(sdk.HotspotsGroupBy.Findings, 'findings');
    const hotspotFindings = await sdk.hotspots({
      ledgerHome,
      groupBy: sdk.HotspotsGroupBy.Findings,
    });
    assert.equal(hotspotFindings.kind, 'findings');
    assert.ok(Array.isArray(hotspotFindings.findings));

    const compare = await sdk.compare({
      ledgerHome,
      models: ['claude-sonnet-4-5', 'claude-opus-4-7'],
      minFidelity: 'partial',
    });
    assert.ok(Array.isArray(compare.cells));
    assert.equal(compare.fidelity.minimum, 'partial');

    const fp = await sdk.fingerprint({ ledgerHome });
    assert.equal(typeof fp.fingerprint, 'string');
    assert.match(fp.fingerprint, /^\d+:\d*:\d+$/);
    // Same input → same fingerprint (stability).
    const fp2 = await sdk.fingerprint({ ledgerHome });
    assert.equal(fp.fingerprint, fp2.fingerprint);
    // Per-session scope differs from global.
    const fpSession = await sdk.fingerprint({
      ledgerHome,
      session: '11111111-1111-1111-1111-111111111111',
    });
    assert.notEqual(fp.fingerprint, fpSession.fingerprint);
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('span tree, flow graph, and context delta verbs return stable shapes', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const ledgerHome = makeLedgerHome();
  const session = '11111111-1111-1111-1111-111111111111';
  try {
    const trees = await sdk.sessionSpanTrees({ sessionId: session, ledgerHome });
    assert.ok(Array.isArray(trees));
    if (trees.length > 0) {
      assert.equal(trees[0].sessionId, session);
      assert.equal(typeof trees[0].turnId, 'string');
      assert.equal(typeof trees[0].root.kind, 'string');
      assert.ok(Array.isArray(trees[0].root.children));

      const single = await sdk.turnSpanTree({
        sessionId: session,
        turnId: trees[0].turnId,
        ledgerHome,
      });
      assert.equal(single.turnId, trees[0].turnId);
      assert.equal(single.root.kind, trees[0].root.kind);
    }

    const empty = await sdk.sessionSpanTrees({
      sessionId: 'not-a-session',
      ledgerHome,
    });
    assert.deepEqual(empty, []);

    await assert.rejects(
      () => sdk.turnSpanTree({ sessionId: session, turnId: 'missing-turn', ledgerHome }),
      /turn not found/,
    );

    const graph = await sdk.flowGraph({ sessionId: session, ledgerHome });
    assert.equal(graph.sessionId, session);
    assert.equal(typeof graph.turnCount, 'number');
    assert.ok(Array.isArray(graph.nodes));
    assert.ok(Array.isArray(graph.edges));
    for (const node of graph.nodes) {
      assert.ok(node.model === null || typeof node.model === 'string');
    }

    const deltas = await sdk.contextDelta({ session, ledgerHome });
    assert.ok(Array.isArray(deltas));
    for (const d of deltas) {
      assert.equal(typeof d.sessionId, 'string');
      assert.equal(typeof d.turnId, 'string');
      assert.equal(typeof d.ownerRail.kind, 'string');
      assert.ok(
        typeof d.priorContextTokens === 'number' || typeof d.priorContextTokens === 'bigint',
      );
      assert.ok(
        typeof d.currentContextTokens === 'number' || typeof d.currentContextTokens === 'bigint',
      );
      assert.ok(typeof d.deltaTokens === 'number' || typeof d.deltaTokens === 'bigint');
      assert.ok(Array.isArray(d.intervening));
    }

    await assert.rejects(
      () => sdk.contextDelta({ ledgerHome, owner: 'both' }),
      /invalid owner/,
    );
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('ledgerFreshness keeps JSONL-only historical imports stale', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;
  clearStaleThresholdEnv(t);

  const ledgerHome = mkdtempSync(join(tmpdir(), 'relayburn-historical-ledger-'));
  try {
    writeFileSync(join(ledgerHome, 'config.json'),
      JSON.stringify({ staleness: { thresholdHours: 24 } }));
    writeFileSync(join(ledgerHome, 'ledger.jsonl'), JSON.stringify({
      kind: 'turn',
      record: {
        v: 1, source: 'codex', sessionId: 'old-session', messageId: 'old-message',
        turnIndex: 0, ts: '2025-01-01T00:00:00.123Z', model: 'gpt-5.2-codex',
        usage: { input: 1, output: 1, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      },
    }) + '\n');
    const freshness = await sdk.ledgerFreshness({ ledgerHome });
    assert.equal(freshness.lastWriteAtMs, Date.parse('2025-01-01T00:00:00.123Z'));
    assert.equal(freshness.stale, true);
    assert.equal((await sdk.summary({ ledgerHome })).turnCount, 1);
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('ledgerFreshness returns null threshold when warnings are disabled', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;
  clearStaleThresholdEnv(t);

  const ledgerHome = makeLedgerHome();
  try {
    writeFileSync(
      join(ledgerHome, 'config.json'),
      JSON.stringify({ staleness: { thresholdHours: -1 } }),
    );
    const freshness = await sdk.ledgerFreshness({ ledgerHome });
    assert.equal(freshness.staleAfterMs, null);
    assert.equal(freshness.stale, false);
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

test('writePendingStamp writes a launcher-safe manifest', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const ledgerHome = mkdtempSync(join(tmpdir(), 'relayburn-sdk-pending-'));
  try {
    const result = await sdk.writePendingStamp({
      ledgerHome,
      harness: 'claude',
      cwd: '/tmp/project',
      enrichment: { persona: 'code-reviewer', agentworkforce: '1' },
      sessionDirHint: '/tmp/project/sessions',
      spawnStartTs: '2026-04-23T00:00:00.000Z',
      spawnerPid: 12345,
    });

    assert.match(result.file, /pending-stamps[/\\]claude-12345-/);
    assert.equal(result.stamp.harness, 'claude');
    assert.equal(result.stamp.enrichment.persona, 'code-reviewer');

    const files = readdirSync(join(ledgerHome, 'pending-stamps'));
    assert.equal(files.length, 1);
    const manifest = JSON.parse(readFileSync(join(ledgerHome, 'pending-stamps', files[0]), 'utf8'));
    assert.equal(manifest.harness, 'claude');
    assert.equal(manifest.enrichment.agentworkforce, '1');
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('writeStamp folds enrichment onto an exact session id', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;

  const ledgerHome = mkdtempSync(join(tmpdir(), 'relayburn-sdk-stamp-'));
  try {
    await sdk.writeStamp({
      ledgerHome,
      sessionId: 'pear-session-7f3b9b4c',
      enrichment: { spawner: 'pear', on_relay: 'true', spawned_by: 'direct' },
    });
    const stamps = await sdk.exportStamps({ ledgerHome });
    assert.equal(stamps.length, 1, 'expected one stamp row');
    const record = stamps[0].record ?? stamps[0];
    assert.equal(record.selector.sessionId, 'pear-session-7f3b9b4c');
    assert.equal(record.enrichment.spawner, 'pear');
    assert.equal(record.enrichment.spawned_by, 'direct');
    assert.equal(record.enrichment.on_relay, 'true');
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('writeStamp rejects empty selector', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;
  const ledgerHome = mkdtempSync(join(tmpdir(), 'relayburn-sdk-stamp-empty-'));
  try {
    await assert.rejects(
      sdk.writeStamp({ ledgerHome, enrichment: { k: 'v' } }),
      /sessionId or messageId/,
    );
  } finally {
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});

test('writeStamp rejects empty enrichment', async (t) => {
  const sdk = await loadNapiSdk(t);
  if (!sdk) return;
  const ledgerHome = mkdtempSync(join(tmpdir(), 'relayburn-sdk-stamp-noenrich-'));
  try {
    await assert.rejects(
      sdk.writeStamp({ ledgerHome, sessionId: 's', enrichment: {} }),
      /enrichment must contain at least one tag/,
    );
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
    assert.equal(typeof report.appliedPendingStamps, 'number');
  } finally {
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
    if (prevUserprofile === undefined) delete process.env.USERPROFILE;
    else process.env.USERPROFILE = prevUserprofile;
    rmSync(fakeHome, { recursive: true, force: true });
    rmSync(ledgerHome, { recursive: true, force: true });
  }
});
