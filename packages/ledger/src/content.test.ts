import { strict as assert } from 'node:assert';
import { mkdtemp, readdir, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import type { ContentRecord } from '@relayburn/reader';

import { loadConfig, DEFAULT_CONFIG } from './config.js';
import {
  __setContentFileMtimeForTesting,
  appendContent,
  pruneContent,
  readContent,
} from './content.js';
import { configPath, contentDir, contentFilePath } from './paths.js';

function record(overrides: Partial<ContentRecord> = {}): ContentRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 'sess-1',
    messageId: 'msg-1',
    ts: '2026-04-20T00:00:00.000Z',
    role: 'assistant',
    kind: 'text',
    text: 'hello world',
    ...overrides,
  };
}

describe('content sidecar', () => {
  let tmp: string;
  const originalHome = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];
  const originalTtl = process.env['RELAYBURN_CONTENT_TTL_DAYS'];

  before(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'relayburn-content-'));
  });

  beforeEach(async () => {
    await rm(tmp, { recursive: true, force: true });
    tmp = await mkdtemp(path.join(tmpdir(), 'relayburn-content-'));
    process.env['RELAYBURN_HOME'] = tmp;
    delete process.env['RELAYBURN_CONTENT_STORE'];
    delete process.env['RELAYBURN_CONTENT_TTL_DAYS'];
  });

  after(async () => {
    if (originalHome !== undefined) process.env['RELAYBURN_HOME'] = originalHome;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    if (originalTtl !== undefined) process.env['RELAYBURN_CONTENT_TTL_DAYS'] = originalTtl;
    else delete process.env['RELAYBURN_CONTENT_TTL_DAYS'];
    await rm(tmp, { recursive: true, force: true });
  });

  it('creates content/ lazily on first append and round-trips records', async () => {
    // Before any append, no content dir exists.
    await assert.rejects(() => stat(contentDir()), { code: 'ENOENT' });

    const toolUseRecord: ContentRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: 's-A',
      messageId: 'm-2',
      ts: '2026-04-20T00:00:00.000Z',
      role: 'assistant',
      kind: 'tool_use',
      toolUse: { id: 't1', name: 'Bash', input: { command: 'ls' } },
    };
    await appendContent([record({ sessionId: 's-A', messageId: 'm-1' }), toolUseRecord]);

    const got = await readContent({ sessionId: 's-A' });
    assert.equal(got.length, 2);
    assert.equal(got[0]!.messageId, 'm-1');
    assert.equal(got[1]!.toolUse!.name, 'Bash');
  });

  it('filters by messageId', async () => {
    await appendContent([
      record({ sessionId: 's-X', messageId: 'a' }),
      record({ sessionId: 's-X', messageId: 'b' }),
    ]);
    const got = await readContent({ sessionId: 's-X', messageId: 'b' });
    assert.equal(got.length, 1);
    assert.equal(got[0]!.messageId, 'b');
  });

  it('returns [] when the session file does not exist', async () => {
    const got = await readContent({ sessionId: 'does-not-exist' });
    assert.deepEqual(got, []);
  });

  it('groups records by sessionId into one file per session', async () => {
    await appendContent([
      record({ sessionId: 's-A', messageId: 'a' }),
      record({ sessionId: 's-B', messageId: 'b' }),
    ]);
    const files = await readdir(contentDir());
    assert.deepEqual(files.sort(), ['s-A.jsonl', 's-B.jsonl']);
  });

  it('pruneContent removes files older than olderThanMs by mtime', async () => {
    await appendContent([
      record({ sessionId: 's-old', messageId: 'm' }),
      record({ sessionId: 's-new', messageId: 'm' }),
    ]);
    // Backdate the old session's file mtime by 120 days.
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await __setContentFileMtimeForTesting('s-old', longAgo);

    const ninety = 90 * 24 * 60 * 60 * 1000;
    const result = await pruneContent({ olderThanMs: ninety });
    assert.equal(result.filesDeleted, 1);
    assert.ok(result.bytesFreed > 0);

    const files = await readdir(contentDir());
    assert.deepEqual(files, ['s-new.jsonl']);
  });

  it('pruneContent returns zero counts when content dir does not exist', async () => {
    const result = await pruneContent({ olderThanMs: 1000 });
    assert.deepEqual(result, { filesDeleted: 0, bytesFreed: 0 });
  });
});

describe('loadConfig', () => {
  let tmp: string;
  const originalHome = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];
  const originalTtl = process.env['RELAYBURN_CONTENT_TTL_DAYS'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'relayburn-cfg-'));
    process.env['RELAYBURN_HOME'] = tmp;
    delete process.env['RELAYBURN_CONTENT_STORE'];
    delete process.env['RELAYBURN_CONTENT_TTL_DAYS'];
  });

  after(async () => {
    if (originalHome !== undefined) process.env['RELAYBURN_HOME'] = originalHome;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    if (originalTtl !== undefined) process.env['RELAYBURN_CONTENT_TTL_DAYS'] = originalTtl;
    else delete process.env['RELAYBURN_CONTENT_TTL_DAYS'];
  });

  it('returns defaults (full, 90) when no env or config file present', async () => {
    const cfg = await loadConfig();
    assert.deepEqual(cfg, DEFAULT_CONFIG);
  });

  it('reads content.store and content.retentionDays from config file', async () => {
    await writeFile(
      configPath(),
      JSON.stringify({ content: { store: 'hash-only', retentionDays: 30 } }),
      'utf8',
    );
    const cfg = await loadConfig();
    assert.equal(cfg.content.store, 'hash-only');
    assert.equal(cfg.content.retentionDays, 30);
  });

  it('env RELAYBURN_CONTENT_STORE wins over config file', async () => {
    await writeFile(
      configPath(),
      JSON.stringify({ content: { store: 'hash-only' } }),
      'utf8',
    );
    process.env['RELAYBURN_CONTENT_STORE'] = 'off';
    const cfg = await loadConfig();
    assert.equal(cfg.content.store, 'off');
  });

  it('env RELAYBURN_CONTENT_TTL_DAYS=forever disables retention', async () => {
    process.env['RELAYBURN_CONTENT_TTL_DAYS'] = 'forever';
    const cfg = await loadConfig();
    assert.equal(cfg.content.retentionDays, 'forever');
  });

  it('env RELAYBURN_CONTENT_TTL_DAYS=-1 disables retention', async () => {
    process.env['RELAYBURN_CONTENT_TTL_DAYS'] = '-1';
    const cfg = await loadConfig();
    assert.equal(cfg.content.retentionDays, 'forever');
  });

  it('ignores invalid content.store values and falls back', async () => {
    process.env['RELAYBURN_CONTENT_STORE'] = 'nonsense';
    const cfg = await loadConfig();
    assert.equal(cfg.content.store, 'full');
  });

  it('writes files with the right sidecar path', async () => {
    assert.equal(contentFilePath('sess-42'), path.join(tmp, 'content', 'sess-42.jsonl'));
  });
});
