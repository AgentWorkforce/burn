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

  it('deduplicates identical records when stream and file ingest overlap', async () => {
    const rec = record({ sessionId: 's-dedupe', messageId: 'm-dedupe' });
    await appendContent([rec]);
    await appendContent([rec]);

    const got = await readContent({ sessionId: 's-dedupe' });
    assert.equal(got.length, 1);
    assert.equal(got[0]!.text, 'hello world');
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
    assert.equal(result.skippedRecoverable, 0);

    const files = await readdir(contentDir());
    assert.deepEqual(files, ['s-new.jsonl']);
  });

  it('pruneContent returns zero counts when content dir does not exist', async () => {
    const result = await pruneContent({ olderThanMs: 1000 });
    assert.deepEqual(result, { filesDeleted: 0, bytesFreed: 0, skippedRecoverable: 0 });
  });

  it('pruneContent with olderThanMs=0 clears the directory (inclusive cutoff)', async () => {
    await appendContent([
      record({ sessionId: 's-1', messageId: 'a' }),
      record({ sessionId: 's-2', messageId: 'b' }),
    ]);
    // Force files to appear older-or-equal to "now" — any eligible mtime
    // relative to cutoff=Date.now().
    const now = new Date();
    await __setContentFileMtimeForTesting('s-1', now);
    await __setContentFileMtimeForTesting('s-2', now);
    const result = await pruneContent({ olderThanMs: 0 });
    assert.equal(result.filesDeleted, 2);
    const files = await readdir(contentDir());
    assert.deepEqual(files, []);
  });

  it('pruneContent skips sessions whose source is recoverable via isRecoverable', async () => {
    await appendContent([
      record({ sessionId: 's-recoverable', messageId: 'm' }),
      record({ sessionId: 's-orphaned', messageId: 'm' }),
    ]);
    // Both files predate the retention cutoff.
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await __setContentFileMtimeForTesting('s-recoverable', longAgo);
    await __setContentFileMtimeForTesting('s-orphaned', longAgo);

    const ninety = 90 * 24 * 60 * 60 * 1000;
    const sources = new Set(['s-recoverable']);
    const result = await pruneContent({
      olderThanMs: ninety,
      isRecoverable: (id) => sources.has(id),
    });

    assert.equal(result.filesDeleted, 1);
    assert.equal(result.skippedRecoverable, 1);
    assert.ok(result.bytesFreed > 0);

    const files = await readdir(contentDir());
    assert.deepEqual(files.sort(), ['s-recoverable.jsonl']);
  });

  it('pruneContent supports an async isRecoverable predicate', async () => {
    await appendContent([record({ sessionId: 's-async', messageId: 'm' })]);
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await __setContentFileMtimeForTesting('s-async', longAgo);

    const ninety = 90 * 24 * 60 * 60 * 1000;
    const result = await pruneContent({
      olderThanMs: ninety,
      isRecoverable: async (id) => {
        await Promise.resolve();
        return id === 's-async';
      },
    });

    assert.equal(result.filesDeleted, 0);
    assert.equal(result.skippedRecoverable, 1);
    const files = await readdir(contentDir());
    assert.deepEqual(files, ['s-async.jsonl']);
  });

  it('pruneContent without isRecoverable applies retention unchanged (force-equivalent)', async () => {
    await appendContent([
      record({ sessionId: 's-orphan-1', messageId: 'm' }),
      record({ sessionId: 's-orphan-2', messageId: 'm' }),
    ]);
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await __setContentFileMtimeForTesting('s-orphan-1', longAgo);
    await __setContentFileMtimeForTesting('s-orphan-2', longAgo);

    const ninety = 90 * 24 * 60 * 60 * 1000;
    // Omitting isRecoverable is the `--force` / no-source-index path.
    const result = await pruneContent({ olderThanMs: ninety });
    assert.equal(result.filesDeleted, 2);
    assert.equal(result.skippedRecoverable, 0);
    const files = await readdir(contentDir());
    assert.deepEqual(files, []);
  });

  it('pruneContent treats a throwing isRecoverable as fail-closed-to-prune', async () => {
    await appendContent([record({ sessionId: 's-throws', messageId: 'm' })]);
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await __setContentFileMtimeForTesting('s-throws', longAgo);

    const ninety = 90 * 24 * 60 * 60 * 1000;
    const result = await pruneContent({
      olderThanMs: ninety,
      isRecoverable: () => {
        throw new Error('source index broken');
      },
    });
    // A broken source index must not let pruned-but-source-still-exists
    // sidecars accumulate forever; we fall through to the existing rule.
    assert.equal(result.filesDeleted, 1);
    assert.equal(result.skippedRecoverable, 0);
  });

  it('rejects content records with path-traversal sessionId instead of writing outside content/', async () => {
    const bad = record({ sessionId: '../escape', messageId: 'm' });
    const origWrite = process.stderr.write.bind(process.stderr);
    let stderr = '';
    process.stderr.write = ((chunk: string | Uint8Array): boolean => {
      stderr += typeof chunk === 'string' ? chunk : chunk.toString();
      return true;
    }) as typeof process.stderr.write;
    try {
      await appendContent([bad, record({ sessionId: 's-safe', messageId: 'm' })]);
    } finally {
      process.stderr.write = origWrite;
    }
    assert.ok(stderr.includes('unsafe sessionId'));
    const files = await readdir(contentDir());
    assert.deepEqual(files, ['s-safe.jsonl']);
    // The escape path must not exist.
    await assert.rejects(() => stat(path.join(tmp, 'escape.jsonl')), { code: 'ENOENT' });
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

  it('treats empty RELAYBURN_CONTENT_TTL_DAYS as unset (does not configure 0-day retention)', async () => {
    // `Number('') === 0` would make an empty env var mean "0 days" — we
    // treat it as not-set and fall through to the default (90 days).
    process.env['RELAYBURN_CONTENT_TTL_DAYS'] = '';
    const cfg = await loadConfig();
    assert.equal(cfg.content.retentionDays, 90);
  });

  it('whitespace-only RELAYBURN_CONTENT_TTL_DAYS is also treated as unset', async () => {
    process.env['RELAYBURN_CONTENT_TTL_DAYS'] = '   ';
    const cfg = await loadConfig();
    assert.equal(cfg.content.retentionDays, 90);
  });

  it('empty env does not override config file retentionDays', async () => {
    await writeFile(
      configPath(),
      JSON.stringify({ content: { retentionDays: 7 } }),
      'utf8',
    );
    process.env['RELAYBURN_CONTENT_TTL_DAYS'] = '';
    const cfg = await loadConfig();
    assert.equal(cfg.content.retentionDays, 7);
  });

  it('ignores invalid content.store values and falls back', async () => {
    process.env['RELAYBURN_CONTENT_STORE'] = 'nonsense';
    const cfg = await loadConfig();
    assert.equal(cfg.content.store, 'full');
  });

  it('warns on invalid JSON in config file and falls back to defaults', async () => {
    await writeFile(configPath(), '{ this is not json', 'utf8');
    const origWrite = process.stderr.write.bind(process.stderr);
    let stderr = '';
    process.stderr.write = ((chunk: string | Uint8Array): boolean => {
      stderr += typeof chunk === 'string' ? chunk : chunk.toString();
      return true;
    }) as typeof process.stderr.write;
    let cfg;
    try {
      cfg = await loadConfig();
    } finally {
      process.stderr.write = origWrite;
    }
    assert.ok(stderr.includes('invalid JSON'));
    assert.deepEqual(cfg, DEFAULT_CONFIG);
  });

  it('writes files with the right sidecar path', async () => {
    assert.equal(contentFilePath('sess-42'), path.join(tmp, 'content', 'sess-42.jsonl'));
  });

  it('contentFilePath rejects invalid sessionIds', () => {
    assert.throws(() => contentFilePath('../escape'));
    assert.throws(() => contentFilePath('has/slash'));
    assert.throws(() => contentFilePath(''));
    assert.throws(() => contentFilePath('.'));
    assert.throws(() => contentFilePath('..'));
  });

  it('contentFilePath rejects sessionIds longer than the filesystem-safe cap', () => {
    // Cap at 128 keeps `${id}.jsonl` and `content.${id}.lock` safely under
    // the common 255-byte filename limit.
    const tooLong = 'a'.repeat(129);
    assert.throws(() => contentFilePath(tooLong));
    // A 128-char id is still accepted.
    const maxOk = 'a'.repeat(128);
    assert.doesNotThrow(() => contentFilePath(maxOk));
  });
});
