// esbuild bundle smoke test — confirms the umbrella's TS facade bundles
// cleanly when an embedder pulls `@relayburn/sdk` into a downstream build.
//
// Strategy:
//   1. Write a tiny fixture script that imports the umbrella (named
//      exports — `summary`, `ingest`, etc.) and references each verb.
//   2. Run esbuild with the same options a real consumer would (Node ESM
//      target, externals for native + Node builtins).
//   3. Assert exit code 0 and no error output. We do *not* execute the
//      bundle — that requires a real native binding (#247-a). The success
//      condition here is "esbuild can resolve and bundle the facade"
//      since that's where 99 % of bundling regressions surface.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SDK_NODE_ROOT = resolve(__dirname, '..');

const FIXTURE_SOURCE = `
import {
  Ledger,
  ingest,
  summary,
  sessionCost,
  overhead,
  overheadTrim,
  hotspots,
  compare,
  computeCompareExcluded,
  search,
  exportLedger,
  exportStamps,
} from '@relayburn/sdk';

// Reference each export so esbuild can't tree-shake them away — keeps the
// smoke test honest about what's actually reachable through the facade.
export const refs = {
  Ledger,
  ingest,
  summary,
  sessionCost,
  overhead,
  overheadTrim,
  hotspots,
  compare,
  computeCompareExcluded,
  search,
  exportLedger,
  exportStamps,
};
`;

test('esbuild bundles the @relayburn/sdk umbrella facade cleanly', async (t) => {
  // Prefer the locally hoisted esbuild (devDep on this package) so the test
  // works without a global install. Skip if it's not yet on disk — the
  // workspace install during CI puts it there.
  let esbuild;
  try {
    esbuild = await import('esbuild');
  } catch (_) {
    t.skip('esbuild not installed — run `pnpm install` first');
    return;
  }

  const work = mkdtempSync(join(tmpdir(), 'relayburn-sdk-bundle-'));
  try {
    const fixturePath = join(work, 'entry.js');
    writeFileSync(fixturePath, FIXTURE_SOURCE);

    const outFile = join(work, 'bundle.mjs');
    const result = await esbuild.build({
      entryPoints: [fixturePath],
      bundle: true,
      format: 'esm',
      platform: 'node',
      target: 'node22',
      outfile: outFile,
      // The native .node addon and Node builtins should stay external — that
      // mirrors what a downstream embedder's bundler config would do.
      external: [
        '@relayburn/sdk-darwin-arm64',
        '@relayburn/sdk-darwin-x64',
        '@relayburn/sdk-linux-arm64-gnu',
        '@relayburn/sdk-linux-x64-gnu',
      ],
      // Resolve `@relayburn/sdk` to the in-tree umbrella.
      alias: {
        '@relayburn/sdk': resolve(SDK_NODE_ROOT, 'src/index.js'),
      },
      logLevel: 'silent',
    });

    assert.equal(result.errors.length, 0, `esbuild errors: ${JSON.stringify(result.errors)}`);
    assert.equal(result.warnings.length, 0, `esbuild warnings: ${JSON.stringify(result.warnings)}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
