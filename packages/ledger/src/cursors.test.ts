import { strict as assert } from 'node:assert';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import { loadCursors, saveCursors } from './cursors.js';
import { cursorsPath } from './paths.js';
import { withLock } from './lock.js';

describe('cursors', () => {
  let tmp: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-cursors-test-'));
  });

  beforeEach(async () => {
    await rm(tmp, { recursive: true, force: true });
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-cursors-test-'));
    process.env['RELAYBURN_HOME'] = tmp;
  });

  after(async () => {
    if (originalHome !== undefined) {
      process.env['RELAYBURN_HOME'] = originalHome;
    } else {
      delete process.env['RELAYBURN_HOME'];
    }
    await rm(tmp, { recursive: true, force: true });
  });

  it('round-trips a claude cursor', async () => {
    await saveCursors({
      '/abs/a.jsonl': { kind: 'claude', inode: 1, offsetBytes: 100, mtimeMs: 123 },
    });
    const got = await loadCursors();
    assert.deepEqual(got, {
      '/abs/a.jsonl': { kind: 'claude', inode: 1, offsetBytes: 100, mtimeMs: 123 },
    });
  });

  it('returns {} when file is missing', async () => {
    assert.deepEqual(await loadCursors(), {});
  });

  it('returns {} when file is malformed', async () => {
    const { writeFile, mkdir } = await import('node:fs/promises');
    await mkdir(path.dirname(cursorsPath()), { recursive: true });
    await writeFile(cursorsPath(), '{not json', 'utf8');
    assert.deepEqual(await loadCursors(), {});
  });

  it('writes atomically (no leftover .tmp in success case)', async () => {
    await saveCursors({
      '/abs/x.jsonl': { kind: 'claude', inode: 1, offsetBytes: 0, mtimeMs: 0 },
    });
    const { readdir } = await import('node:fs/promises');
    const entries = await readdir(path.dirname(cursorsPath()));
    for (const e of entries) {
      assert.ok(!e.endsWith('.tmp'), `unexpected tmp file: ${e}`);
    }
  });

  it('withLock serializes concurrent writers', async () => {
    let inside = 0;
    let maxInside = 0;
    async function critical(): Promise<void> {
      inside++;
      maxInside = Math.max(maxInside, inside);
      await new Promise((r) => setTimeout(r, 30));
      inside--;
    }
    await Promise.all([
      withLock('test-serialize', critical),
      withLock('test-serialize', critical),
      withLock('test-serialize', critical),
    ]);
    assert.equal(maxInside, 1, 'lock must serialize critical sections');
  });

  it('preserves JSON shape', async () => {
    await saveCursors({
      '/a': {
        kind: 'codex',
        inode: 2,
        offsetBytes: 0,
        mtimeMs: 0,
        cumulative: { input: 0, output: 0, cacheRead: 0, reasoning: 0 },
        sessionId: 'sess',
        turnContexts: {},
      },
    });
    const raw = await readFile(cursorsPath(), 'utf8');
    const parsed = JSON.parse(raw);
    assert.ok(parsed.files['/a'].kind === 'codex');
  });
});
