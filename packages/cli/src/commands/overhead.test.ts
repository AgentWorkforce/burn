import { strict as assert } from 'node:assert';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import { __resetIndexCacheForTesting, appendTurns } from '@relayburn/ledger';
import type { TurnRecord } from '@relayburn/reader';

import { runOverhead, type OverheadDeps } from './overhead.js';
import type { ParsedArgs } from '../args.js';

// Stop runOverhead from triggering a real `ingestAll()` against the user's
// session stores during tests — that read can take minutes and pollutes the
// isolated tmp ledger with unrelated data.
const NOOP_DEPS: OverheadDeps = {
  ingestAll: async () => ({ scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 }),
};

interface CapturedOutput {
  stdout: string;
  stderr: string;
  code: number;
}

const tmpPaths: string[] = [];
let ledgerHome: string;
const originalHome = process.env['RELAYBURN_HOME'];

beforeEach(async () => {
  ledgerHome = await mkdtemp(path.join(tmpdir(), 'burn-overhead-ledger-'));
  process.env['RELAYBURN_HOME'] = ledgerHome;
  __resetIndexCacheForTesting();
});

afterEach(async () => {
  if (originalHome !== undefined) process.env['RELAYBURN_HOME'] = originalHome;
  else delete process.env['RELAYBURN_HOME'];
  __resetIndexCacheForTesting();
  await rm(ledgerHome, { recursive: true, force: true });
  await Promise.all(tmpPaths.splice(0).map((p) => rm(p, { recursive: true, force: true })));
});

function args(
  positional: string[] = [],
  flags: Record<string, string | true> = {},
): ParsedArgs {
  return { flags, tags: {}, positional, passthrough: [] };
}

async function captureOverhead(parsed: ParsedArgs): Promise<CapturedOutput> {
  const origStdout = process.stdout.write.bind(process.stdout);
  const origStderr = process.stderr.write.bind(process.stderr);
  let stdout = '';
  let stderr = '';
  process.stdout.write = ((chunk: string | Uint8Array): boolean => {
    stdout += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((chunk: string | Uint8Array): boolean => {
    stderr += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stderr.write;
  try {
    const code = await runOverhead(parsed, NOOP_DEPS);
    return { stdout, stderr, code };
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
}

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-overhead-json',
    messageId: 'm-overhead-json-1',
    turnIndex: 0,
    ts: '2026-04-29T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: {
      input: 100,
      output: 50,
      reasoning: 0,
      cacheRead: 100_000,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    project: '/tmp/project',
    ...overrides,
  };
}

async function makeProject(fileText: string): Promise<string> {
  const projectPath = await mkdtemp(path.join(tmpdir(), 'burn-overhead-json-'));
  tmpPaths.push(projectPath);
  await writeFile(path.join(projectPath, 'CLAUDE.md'), fileText, 'utf8');
  return projectPath;
}

describe('burn overhead trim --json', () => {
  it('emits structured trim recommendations with embedded unified diffs', async () => {
    const projectPath = await makeProject(
      [
        '# Project',
        '',
        '## Big',
        'x'.repeat(8000),
        '## Small',
        'short section',
      ].join('\n'),
    );
    await appendTurns([fakeTurn({ project: projectPath })]);

    const out = await captureOverhead(
      args(['trim'], { project: projectPath, since: '30d', top: '1', json: true }),
    );

    assert.equal(out.code, 0);
    assert.equal(out.stderr, '');
    const payload = JSON.parse(out.stdout);
    assert.equal(payload.project, projectPath);
    assert.equal(payload.since, '30d');
    assert.deepEqual(payload.summary, {
      filesAnalyzed: 1,
      filesWithRecommendations: 1,
      totalRecommendations: 1,
      totalProjectedSavingsPerSession: payload.recommendations[0].projectedSavings.perSessionUsd,
      totalProjectedSavingsAcrossWindow: payload.recommendations[0].projectedSavings.acrossWindowUsd,
    });

    const rec = payload.recommendations[0];
    assert.equal(rec.file, 'CLAUDE.md');
    assert.equal(rec.kind, 'claude-md');
    assert.deepEqual(rec.appliesTo, ['claude-code']);
    assert.deepEqual(rec.section, {
      heading: '## Big',
      startLine: 3,
      endLine: 4,
      tokens: rec.projectedSavings.tokens,
    });
    assert.equal(typeof rec.projectedSavings.perSessionUsd, 'number');
    assert.ok(rec.projectedSavings.perSessionUsd > 0);
    assert.equal(typeof rec.projectedSavings.acrossWindowUsd, 'number');
    assert.ok(rec.projectedSavings.acrossWindowUsd > 0);
    assert.equal(typeof rec.projectedSavings.tokenShare, 'number');
    assert.ok(rec.projectedSavings.tokenShare > 0);
    assert.match(rec.diff, /^# TRIM: ## Big\n/);
    assert.match(rec.diff, /\n--- a\/CLAUDE\.md\n\+\+\+ b\/CLAUDE\.md\n@@ -3,2 \+3,0 @@\n/);
  });

  it('emits an empty recommendations array when files have no headed trim candidates', async () => {
    const projectPath = await makeProject('plain instructions\nwithout headings\n');
    await appendTurns([fakeTurn({ project: projectPath })]);

    const out = await captureOverhead(
      args(['trim'], { project: projectPath, json: true }),
    );

    assert.equal(out.code, 0);
    assert.equal(out.stderr, '');
    const payload = JSON.parse(out.stdout);
    assert.equal(payload.project, projectPath);
    assert.equal(payload.since, 'all time');
    assert.deepEqual(payload.recommendations, []);
    assert.deepEqual(payload.summary, {
      filesAnalyzed: 1,
      filesWithRecommendations: 0,
      totalRecommendations: 0,
      totalProjectedSavingsPerSession: 0,
      totalProjectedSavingsAcrossWindow: 0,
    });
  });
});
