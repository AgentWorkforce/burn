import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { afterEach, beforeEach, describe, it } from 'node:test';

const cliPath = path.join(path.dirname(fileURLToPath(import.meta.url)), 'cli.js');

interface CliResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

describe('burn CLI state dispatch', () => {
  let tmp: string;
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-cli-dispatch-'));
    process.env['RELAYBURN_HOME'] = tmp;
    process.env['RELAYBURN_CONTENT_STORE'] = 'off';
  });

  afterEach(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    await rm(tmp, { recursive: true, force: true });
  });

  function runCli(args: string[]): CliResult {
    const result = spawnSync(process.execPath, [cliPath, ...args], {
      encoding: 'utf8',
      env: {
        ...process.env,
        RELAYBURN_HOME: tmp,
        RELAYBURN_CONTENT_STORE: 'off',
      },
    });
    return {
      status: result.status,
      stdout: result.stdout,
      stderr: result.stderr,
    };
  }

  it('burn state defaults to status', () => {
    const out = runCli(['state']);
    assert.equal(out.status, 0);
    assert.match(out.stdout, /derived state:/);
    assert.match(out.stdout, /archive:/);
  });

  it('burn state rebuild without a target prints help and exits non-zero', () => {
    const out = runCli(['state', 'rebuild']);
    assert.equal(out.status, 2);
    assert.match(out.stderr, /missing target/);
    assert.match(out.stderr, /burn state rebuild/);
  });

  it('burn state rebuild index dispatches to the index target', () => {
    const out = runCli(['state', 'rebuild', 'index']);
    assert.equal(out.status, 0);
    assert.match(out.stdout, /rebuilt ledger index/);
  });

  it('burn state prune dispatches to content pruning', () => {
    const out = runCli(['state', 'prune', '--days', 'forever']);
    assert.equal(out.status, 0);
    assert.match(out.stdout, /retention=forever/);
  });

  it('burn state rejects unknown subcommands', () => {
    const out = runCli(['state', 'nope']);
    assert.notEqual(out.status, 0);
    assert.match(out.stderr, /unknown subcommand/);
  });

  it('does not retain top-level rebuild or content dispatch aliases', () => {
    const rebuild = runCli(['rebuild', 'status']);
    const content = runCli(['content', 'prune']);
    assert.notEqual(rebuild.status, 0);
    assert.match(rebuild.stderr, /unknown command: rebuild/);
    assert.notEqual(content.status, 0);
    assert.match(content.stderr, /unknown command: content/);
  });
});
