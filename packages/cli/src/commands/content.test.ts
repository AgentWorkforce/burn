import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { runContent } from './content.js';

describe('burn content prune CLI', () => {
  let tmp: string;
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-content-cli-'));
    process.env['RELAYBURN_HOME'] = tmp;
    delete process.env['RELAYBURN_CONTENT_STORE'];
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
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
});
