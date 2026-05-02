import { strict as assert } from 'node:assert';
import { mkdtemp, readFile, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { ledgerPath } from '@relayburn/ledger';

import {
  cleanupStalePendingStamps,
  pendingStampsDir,
  resolvePendingStampsForSession,
  writePendingStamp,
} from './pending-stamps.js';

describe('pending stamp manifest', () => {
  let tmpRelay: string;
  const originalRelay = process.env['RELAYBURN_HOME'];

  beforeEach(async () => {
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-pending-stamps-'));
    process.env['RELAYBURN_HOME'] = tmpRelay;
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('writes the v1 manifest under RELAYBURN_HOME/pending-stamps', async () => {
    const result = await writePendingStamp({
      harness: 'codex',
      cwd: '/tmp/project',
      enrichment: { workflowId: 'wf-1', agentId: 'ag-1' },
      sessionDirHint: '/tmp/codex/sessions',
      spawnStartTs: new Date('2026-04-24T12:00:00.000Z'),
      spawnerPid: 63,
    });

    assert.equal(path.dirname(result.file), pendingStampsDir());
    const files = await readdir(pendingStampsDir());
    assert.equal(files.length, 1);
    const parsed = JSON.parse(await readFile(result.file, 'utf8')) as {
      v: number;
      harness: string;
      spawnerPid: number;
      spawnStartTs: string;
      cwd: string;
      enrichment: Record<string, string>;
      sessionDirHint: string;
    };
    assert.equal(parsed.v, 1);
    assert.equal(parsed.harness, 'codex');
    assert.equal(parsed.spawnerPid, 63);
    assert.equal(parsed.spawnStartTs, '2026-04-24T12:00:00.000Z');
    assert.equal(parsed.cwd, '/tmp/project');
    assert.deepEqual(parsed.enrichment, { workflowId: 'wf-1', agentId: 'ag-1' });
    assert.equal(parsed.sessionDirHint, '/tmp/codex/sessions');
  });

  it('claims and resolves a matching manifest once', async () => {
    const spawnStart = new Date();
    await writePendingStamp({
      harness: 'codex',
      cwd: '/tmp/project',
      enrichment: { workflowId: 'wf-once' },
      sessionDirHint: '/tmp/codex/sessions',
      spawnStartTs: spawnStart,
      spawnerPid: 63,
    });

    const first = await resolvePendingStampsForSession({
      harness: 'codex',
      sessionId: 'sess_once',
      sessionPath: '/tmp/codex/sessions/2026/04/24/renamed.jsonl',
      sessionMtimeMs: spawnStart.getTime() + 10,
      cwd: '/tmp/project',
    });
    const second = await resolvePendingStampsForSession({
      harness: 'codex',
      sessionId: 'sess_once',
      sessionPath: '/tmp/codex/sessions/2026/04/24/renamed.jsonl',
      sessionMtimeMs: spawnStart.getTime() + 10,
      cwd: '/tmp/project',
    });

    assert.equal(first.applied, 1);
    assert.equal(second.applied, 0);
    assert.deepEqual(await listPendingFiles(), []);

    const ledgerLines = (await readFile(ledgerPath(), 'utf8')).trim().split('\n');
    assert.equal(ledgerLines.length, 1);
    const line = JSON.parse(ledgerLines[0]!) as {
      kind: string;
      selector: { sessionId?: string };
      enrichment: Record<string, string>;
    };
    assert.equal(line.kind, 'stamp');
    assert.equal(line.selector.sessionId, 'sess_once');
    assert.equal(line.enrichment['workflowId'], 'wf-once');
  });

  it('does not cross-contaminate two same-cwd same-harness runs', async () => {
    const now = Date.now();
    const spawnStartA = new Date(now);
    const spawnStartB = new Date(now + 1000);
    await writePendingStamp({
      harness: 'codex',
      cwd: '/tmp/project',
      enrichment: { tag: 'A' },
      sessionDirHint: '/tmp/codex/sessions',
      spawnStartTs: spawnStartA,
      spawnerPid: 100,
    });
    await writePendingStamp({
      harness: 'codex',
      cwd: '/tmp/project',
      enrichment: { tag: 'B' },
      sessionDirHint: '/tmp/codex/sessions',
      spawnStartTs: spawnStartB,
      spawnerPid: 101,
    });

    const first = await resolvePendingStampsForSession({
      harness: 'codex',
      sessionId: 'sess_first',
      sessionPath: '/tmp/codex/sessions/2026/04/24/first.jsonl',
      sessionMtimeMs: spawnStartB.getTime() + 50,
      cwd: '/tmp/project',
    });
    const second = await resolvePendingStampsForSession({
      harness: 'codex',
      sessionId: 'sess_second',
      sessionPath: '/tmp/codex/sessions/2026/04/24/second.jsonl',
      sessionMtimeMs: spawnStartB.getTime() + 60,
      cwd: '/tmp/project',
    });

    assert.equal(first.applied, 1);
    assert.equal(second.applied, 1);
    // Oldest stamp (A) goes to whichever session ingests first.
    assert.equal(first.enrichment['tag'], 'A');
    assert.equal(second.enrichment['tag'], 'B');
    assert.deepEqual(await listPendingFiles(), []);

    const stampLines = (await readFile(ledgerPath(), 'utf8'))
      .trim()
      .split('\n')
      .map((l) => JSON.parse(l) as { selector: { sessionId?: string }; enrichment: Record<string, string> });
    assert.equal(stampLines.length, 2);
    const bySession = new Map(stampLines.map((l) => [l.selector.sessionId, l.enrichment['tag']]));
    assert.equal(bySession.get('sess_first'), 'A');
    assert.equal(bySession.get('sess_second'), 'B');
  });

  it('uses mtime causality as a fallback when session cwd is unavailable', async () => {
    const spawnStart = new Date();
    await writePendingStamp({
      harness: 'opencode',
      cwd: '/tmp/project',
      enrichment: { workflowId: 'wf-mtime' },
      sessionDirHint: '/tmp/opencode/session',
      spawnStartTs: spawnStart,
      spawnerPid: 63,
    });

    const missed = await resolvePendingStampsForSession({
      harness: 'opencode',
      sessionId: 'ses_old',
      sessionPath: '/tmp/opencode/session/global/ses_old.json',
      sessionMtimeMs: spawnStart.getTime() - 10,
    });
    const matched = await resolvePendingStampsForSession({
      harness: 'opencode',
      sessionId: 'ses_new',
      sessionPath: '/tmp/opencode/session/global/ses_new.json',
      sessionMtimeMs: spawnStart.getTime() + 10,
    });

    assert.equal(missed.applied, 0);
    assert.equal(matched.applied, 1);
  });

  it('deletes stale manifests by TTL', async () => {
    await writePendingStamp({
      harness: 'codex',
      cwd: '/tmp/project',
      enrichment: { workflowId: 'wf-stale' },
      spawnStartTs: new Date('2026-04-20T00:00:00.000Z'),
      spawnerPid: 63,
    });

    const result = await cleanupStalePendingStamps({
      now: new Date('2026-04-21T00:00:01.000Z'),
    });

    assert.equal(result.deleted, 1);
    assert.deepEqual(await listPendingFiles(), []);
  });
});

async function listPendingFiles(): Promise<string[]> {
  try {
    return await readdir(pendingStampsDir());
  } catch {
    return [];
  }
}
