import { strict as assert } from 'node:assert';
import { mkdtemp, rm, utimes } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { runContent } from './content.js';

describe('burn content prune CLI', () => {
  let tmp: string;
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];
  const originalForce = process.env['RELAYBURN_PRUNE_FORCE'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-content-cli-'));
    process.env['RELAYBURN_HOME'] = tmp;
    delete process.env['RELAYBURN_CONTENT_STORE'];
    delete process.env['RELAYBURN_PRUNE_FORCE'];
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    if (originalForce !== undefined) process.env['RELAYBURN_PRUNE_FORCE'] = originalForce;
    else delete process.env['RELAYBURN_PRUNE_FORCE'];
    await rm(tmp, { recursive: true, force: true });
  });

  it('returns non-zero and prints help on invalid --days instead of throwing', async () => {
    const origWrite = process.stderr.write.bind(process.stderr);
    const origStdout = process.stdout.write.bind(process.stdout);
    let stderr = '';
    process.stderr.write = ((chunk: string | Uint8Array): boolean => {
      stderr += typeof chunk === 'string' ? chunk : chunk.toString();
      return true;
    }) as typeof process.stderr.write;
    process.stdout.write = ((_chunk: string | Uint8Array): boolean => true) as typeof process.stdout.write;
    let code: number;
    try {
      code = await runContent({
        flags: { days: 'not-a-number' },
        tags: {},
        positional: ['prune'],
        passthrough: [],
      });
    } finally {
      process.stderr.write = origWrite;
      process.stdout.write = origStdout;
    }
    assert.equal(code, 2);
    assert.ok(stderr.includes('invalid --days value'));
  });

  it('accepts numeric --days and exits 0', async () => {
    const origStdout = process.stdout.write.bind(process.stdout);
    process.stdout.write = ((_chunk: string | Uint8Array): boolean => true) as typeof process.stdout.write;
    let code: number;
    try {
      code = await runContent({
        flags: { days: '30' },
        tags: {},
        positional: ['prune'],
        passthrough: [],
      });
    } finally {
      process.stdout.write = origStdout;
    }
    assert.equal(code, 0);
  });

  it('accepts forever and exits 0', async () => {
    const origStdout = process.stdout.write.bind(process.stdout);
    process.stdout.write = ((_chunk: string | Uint8Array): boolean => true) as typeof process.stdout.write;
    let code: number;
    try {
      code = await runContent({
        flags: { days: 'forever' },
        tags: {},
        positional: ['prune'],
        passthrough: [],
      });
    } finally {
      process.stdout.write = origStdout;
    }
    assert.equal(code, 0);
  });

  it('--force bypasses the recoverable check and deletes per retention', async () => {
    // Stage a sidecar with a backdated mtime so retention would otherwise
    // delete it. With --force the source-index check is skipped entirely,
    // and the file is removed regardless of whether the upstream session
    // file still exists on the host.
    const { appendContent } = await import('@relayburn/ledger');
    await appendContent([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-force',
        messageId: 'm',
        ts: '2026-04-20T00:00:00.000Z',
        role: 'assistant',
        kind: 'text',
        text: 'hi',
      },
    ]);
    const sidecar = path.join(tmp, 'content', 's-force.jsonl');
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await utimes(sidecar, longAgo, longAgo);

    const origStdout = process.stdout.write.bind(process.stdout);
    let stdout = '';
    process.stdout.write = ((chunk: string | Uint8Array): boolean => {
      stdout += typeof chunk === 'string' ? chunk : chunk.toString();
      return true;
    }) as typeof process.stdout.write;
    let code: number;
    try {
      code = await runContent({
        flags: { days: '90', force: true },
        tags: {},
        positional: ['prune'],
        passthrough: [],
      });
    } finally {
      process.stdout.write = origStdout;
    }
    assert.equal(code, 0);
    assert.match(stdout, /pruned 1 content file/);
    // With --force the recoverable line must be suppressed.
    assert.doesNotMatch(stdout, /kept .* recoverable/);
  });

  it('RELAYBURN_PRUNE_FORCE=1 env var bypasses the recoverable check', async () => {
    const { appendContent } = await import('@relayburn/ledger');
    await appendContent([
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's-env-force',
        messageId: 'm',
        ts: '2026-04-20T00:00:00.000Z',
        role: 'assistant',
        kind: 'text',
        text: 'hi',
      },
    ]);
    const sidecar = path.join(tmp, 'content', 's-env-force.jsonl');
    const longAgo = new Date(Date.now() - 120 * 24 * 60 * 60 * 1000);
    await utimes(sidecar, longAgo, longAgo);

    process.env['RELAYBURN_PRUNE_FORCE'] = '1';
    const origStdout = process.stdout.write.bind(process.stdout);
    let stdout = '';
    process.stdout.write = ((chunk: string | Uint8Array): boolean => {
      stdout += typeof chunk === 'string' ? chunk : chunk.toString();
      return true;
    }) as typeof process.stdout.write;
    let code: number;
    try {
      code = await runContent({
        flags: { days: '90' },
        tags: {},
        positional: ['prune'],
        passthrough: [],
      });
    } finally {
      process.stdout.write = origStdout;
      delete process.env['RELAYBURN_PRUNE_FORCE'];
    }
    assert.equal(code, 0);
    assert.match(stdout, /pruned 1 content file/);
    assert.doesNotMatch(stdout, /kept .* recoverable/);
  });
});
