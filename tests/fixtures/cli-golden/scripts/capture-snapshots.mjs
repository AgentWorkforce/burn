#!/usr/bin/env node
// Re-run every TS-CLI invocation in invocations.json against the fixture
// ledger and write the captured stdout/stderr to snapshots/.
//
// The ledger is rebuilt fresh on every run (build-ledger.mjs), then the CLI
// is shelled out from packages/cli/dist/cli.js with a sealed env:
//   - RELAYBURN_HOME points at tests/fixtures/cli-golden/ledger
//   - HOME points at a tmp dir with no .claude / .codex / .local trees, so
//     ingestAll's session-store sweep finds zero work
//   - RELAYBURN_CONTENT_STORE=off so the content sidecar isn't materialized
//   - RELAYBURN_ARCHIVE_AUTOBUILD=0 so summary doesn't autobuild the archive
//
// Snapshots are written verbatim from stdout, with two normalizations:
//   1. the absolute fixture HOME path becomes ${RELAYBURN_HOME}
//   2. the absolute fixture project path becomes ${PROJECT}
// Wave 2 PRs comparing Rust output do the same substitution before diffing
// so snapshots stay portable across machines / CI runners.

import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, '..', '..', '..', '..');
const FIXTURE_DIR = path.resolve(__dirname, '..');
const LEDGER_HOME = path.join(FIXTURE_DIR, 'ledger');
const PROJECT_DIR = path.join(FIXTURE_DIR, 'project');
const SNAPSHOT_DIR = path.join(FIXTURE_DIR, 'snapshots');
const INVOCATIONS = path.join(FIXTURE_DIR, 'invocations.json');
const CLI_PATH = path.join(ROOT, 'packages', 'cli', 'dist', 'cli.js');

await mkdir(LEDGER_HOME, { recursive: true });
await mkdir(SNAPSHOT_DIR, { recursive: true });

// Step 1 — wipe + rebuild the fixture ledger.
console.error(`[capture] (re)building fixture ledger at ${LEDGER_HOME}`);
const buildResult = spawnSync(
  process.execPath,
  [path.join(__dirname, 'build-ledger.mjs')],
  {
    encoding: 'utf8',
    env: { ...process.env, RELAYBURN_HOME: LEDGER_HOME, RELAYBURN_CONTENT_STORE: 'off' },
    stdio: ['ignore', 'inherit', 'inherit'],
  },
);
if (buildResult.status !== 0) {
  process.stderr.write(`[capture] build-ledger failed (status=${buildResult.status})\n`);
  process.exit(1);
}

// Step 2 — sealed HOME with no agent session stores. ingestAll's listDirs
// returns [] for missing dirs so this keeps every read-path command's "ingest"
// preamble at "ingested 0 new sessions" without needing to mock it out.
const SEALED_HOME = await mkdtemp(path.join(tmpdir(), 'burn-golden-home-'));

// Step 3 — load the invocations contract and run each.
const invocations = JSON.parse(await readFile(INVOCATIONS, 'utf8'));

let failures = 0;
for (const inv of invocations) {
  const args = inv.args;
  const env = {
    ...process.env,
    HOME: SEALED_HOME,
    RELAYBURN_HOME: LEDGER_HOME,
    RELAYBURN_CONTENT_STORE: 'off',
    // Force the streaming-ledger fallback in `burn summary` / `burn compare`.
    // The archive is a perf optimization that materializes a SQLite mirror;
    // its build path can hit binding errors on hand-rolled fixtures, and
    // either way the output is meant to be identical to the streaming path.
    // Wave 2 PRs porting the Rust commands likewise won't have an archive
    // implementation on day one.
    RELAYBURN_ARCHIVE: '0',
    NO_COLOR: '1',
    FORCE_COLOR: '0',
    ...(inv.env ?? {}),
  };
  console.error(`[capture] ${inv.name}: burn ${args.join(' ')}`);
  const result = spawnSync(process.execPath, [CLI_PATH, ...args], {
    encoding: 'utf8',
    env,
    cwd: ROOT,
    timeout: 30_000,
  });
  if (result.error) {
    process.stderr.write(`[capture] ${inv.name}: spawn error ${result.error.message}\n`);
    failures++;
    continue;
  }
  const expectedStatus = typeof inv.expectStatus === 'number' ? inv.expectStatus : 0;
  if (result.status !== expectedStatus) {
    process.stderr.write(
      `[capture] ${inv.name}: expected status ${expectedStatus}, got ${result.status}\n` +
        `  stderr:\n${result.stderr}\n`,
    );
    failures++;
    continue;
  }
  const stdout = normalize(result.stdout, LEDGER_HOME, PROJECT_DIR);
  const stderr = normalize(result.stderr, LEDGER_HOME, PROJECT_DIR);

  await writeFile(path.join(SNAPSHOT_DIR, `${inv.name}.stdout.txt`), stdout);
  if (stderr.length > 0) {
    await writeFile(path.join(SNAPSHOT_DIR, `${inv.name}.stderr.txt`), stderr);
  } else {
    await rm(path.join(SNAPSHOT_DIR, `${inv.name}.stderr.txt`), { force: true });
  }
}

await rm(SEALED_HOME, { recursive: true, force: true });

if (failures > 0) {
  process.stderr.write(`[capture] ${failures} invocation(s) failed\n`);
  process.exit(1);
}
console.error('[capture] done');

/**
 * Replace the absolute LEDGER_HOME path with the placeholder ${RELAYBURN_HOME}
 * and the absolute project path with ${PROJECT}, so snapshots are portable
 * across machines / CI runners. The diff runner applies the same substitution
 * before comparing. Wall-clock millisecond fields in the `state status --json`
 * shape (`ledgerMtimeMsCurrent`, `lastBuiltAt`, `lastRebuildAt`) are squashed
 * to a stable placeholder for the same reason.
 */
function normalize(text, ledgerHome, projectDir) {
  let out = text.replaceAll(ledgerHome, '${RELAYBURN_HOME}').replaceAll(projectDir, '${PROJECT}');
  // Squash wall-clock millisecond fields — they're load-bearing for cache
  // invalidation but have no business in a golden snapshot.
  out = out.replaceAll(
    /"ledgerMtimeMsCurrent":\s*\d+/g,
    '"ledgerMtimeMsCurrent": "${MTIME}"',
  );
  out = out.replaceAll(/"lastBuiltAt":\s*\d+/g, '"lastBuiltAt": "${TS}"');
  out = out.replaceAll(/"lastRebuildAt":\s*\d+/g, '"lastRebuildAt": "${TS}"');
  return out;
}
