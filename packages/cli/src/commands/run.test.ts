import { strict as assert } from 'node:assert';
import { EventEmitter } from 'node:events';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import type { ParsedArgs } from '../args.js';
import type { IngestReport } from '../ingest.js';
import type { HarnessAdapter } from '../harnesses/types.js';
import type { WatchController } from './watch.js';

import { runDeprecatedAlias, runWithAdapter } from './run.js';

function emptyArgs(passthrough: string[] = []): ParsedArgs {
  return { flags: {}, tags: {}, positional: [], passthrough };
}

function fakeChild(exitCode: number): EventEmitter & { exitCode: number } {
  const ee = new EventEmitter() as EventEmitter & { exitCode: number };
  ee.exitCode = exitCode;
  setImmediate(() => ee.emit('exit', exitCode));
  return ee;
}

describe('runWithAdapter', () => {
  it('drives plan → beforeSpawn → spawn → afterExit and propagates exit code', async () => {
    const calls: string[] = [];
    const seenEnv: Record<string, string | undefined> = {};

    const adapter: HarnessAdapter = {
      name: 'fake',
      sessionRoot: () => '/dev/null',
      async plan(ctx) {
        calls.push('plan');
        assert.deepEqual(ctx.passthrough, ['--resume']);
        assert.equal(ctx.tags['harness'], 'fake');
        assert.equal(ctx.tags['burnSpawn'], '1');
        assert.ok(ctx.tags['burnSpawnTs']);
        return { binary: 'fake-bin', args: ['x', '--resume'] };
      },
      async beforeSpawn() {
        calls.push('beforeSpawn');
      },
      async afterExit() {
        calls.push('afterExit');
        return { scannedSessions: 1, ingestedSessions: 1, appendedTurns: 7 };
      },
    };

    const code = await runWithAdapter(adapter, emptyArgs(['--resume']), {
      spawn(bin, args, options) {
        calls.push('spawn');
        assert.equal(bin, 'fake-bin');
        assert.deepEqual([...args], ['x', '--resume']);
        const env = options.env as Record<string, string | undefined>;
        seenEnv['harness'] = env['RELAYBURN_AGENT_ID'] ?? '<unset>';
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        return fakeChild(42) as any;
      },
    });

    assert.equal(code, 42);
    assert.deepEqual(calls, ['plan', 'beforeSpawn', 'spawn', 'afterExit']);
  });

  it('runs the optional watcher around spawn and folds its reports into the total', async () => {
    const calls: string[] = [];
    let watcherTicked = false;
    let watcherStopped = false;

    const watcherReports: Array<(r: IngestReport) => void> = [];
    const watcher: WatchController = {
      async tick() {
        watcherTicked = true;
        // Simulate the watch loop seeing 2 sessions / 5 turns mid-flight.
        for (const cb of watcherReports) cb({ scannedSessions: 0, ingestedSessions: 2, appendedTurns: 5 });
      },
      async stop() {
        watcherStopped = true;
      },
    };

    const adapter: HarnessAdapter = {
      name: 'fake-watching',
      sessionRoot: () => '/dev/null',
      async plan(ctx) {
        calls.push('plan');
        return { binary: 'fake-bin', args: [...ctx.passthrough] };
      },
      async beforeSpawn() {
        calls.push('beforeSpawn');
      },
      startWatcher(_ctx, onReport) {
        calls.push('startWatcher');
        watcherReports.push(onReport);
        return watcher;
      },
      async afterExit() {
        calls.push('afterExit');
        return { scannedSessions: 1, ingestedSessions: 1, appendedTurns: 3 };
      },
    };

    const stderrChunks: string[] = [];
    const origWrite = process.stderr.write.bind(process.stderr);
    process.stderr.write = ((chunk: string | Uint8Array): boolean => {
      stderrChunks.push(typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8'));
      return true;
    }) as typeof process.stderr.write;

    let code: number;
    try {
      code = await runWithAdapter(adapter, emptyArgs([]), {
        spawn() {
          calls.push('spawn');
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          return fakeChild(0) as any;
        },
      });
    } finally {
      process.stderr.write = origWrite;
    }

    assert.equal(code, 0);
    assert.deepEqual(calls, ['plan', 'beforeSpawn', 'startWatcher', 'spawn', 'afterExit']);
    assert.equal(watcherTicked, true);
    assert.equal(watcherStopped, true);

    // Final unified report folds watcher (+2 sessions / +5 turns) and afterExit (+1 / +3).
    const reportLine = stderrChunks.find((s) => s.includes('fake-watching ingest:'));
    assert.ok(reportLine, `expected report line in: ${stderrChunks.join('|')}`);
    assert.match(reportLine!, /fake-watching ingest: 3 sessions \(\+8 turns\)/);
  });

  it('runDeprecatedAlias prints a deprecation warning and delegates to the harness', async () => {
    // The codex adapter writes a pending stamp under RELAYBURN_HOME; isolate to
    // a tmpdir so the test does not touch the developer's real ledger.
    const tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-relay-run-'));
    const tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-home-run-'));
    const originalRelay = process.env['RELAYBURN_HOME'];
    const originalHome = process.env['HOME'];
    process.env['RELAYBURN_HOME'] = tmpRelay;
    process.env['HOME'] = tmpHome;

    const stderrChunks: string[] = [];
    const origWrite = process.stderr.write.bind(process.stderr);
    process.stderr.write = ((chunk: string | Uint8Array): boolean => {
      stderrChunks.push(typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8'));
      return true;
    }) as typeof process.stderr.write;

    let spawnedBin = '';
    let code: number;
    try {
      code = await runDeprecatedAlias('codex', emptyArgs(['--help']), {
        spawn(bin) {
          spawnedBin = bin;
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          return fakeChild(0) as any;
        },
      });
    } finally {
      process.stderr.write = origWrite;
      if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
      else delete process.env['RELAYBURN_HOME'];
      if (originalHome !== undefined) process.env['HOME'] = originalHome;
      else delete process.env['HOME'];
      await rm(tmpRelay, { recursive: true, force: true });
      await rm(tmpHome, { recursive: true, force: true });
    }

    assert.equal(code, 0);
    assert.equal(spawnedBin, 'codex');
    assert.ok(
      stderrChunks.some((s) => s.includes('`burn codex` is deprecated; use `burn run codex`')),
      `expected deprecation warning, got: ${stderrChunks.join('|')}`,
    );
  });

  it('runDeprecatedAlias rejects unknown harness names with exit code 2', async () => {
    const code = await runDeprecatedAlias('cursor', emptyArgs([]));
    assert.equal(code, 2);
  });

  it('returns 127 when the child fails to spawn', async () => {
    const adapter: HarnessAdapter = {
      name: 'fake',
      sessionRoot: () => '/dev/null',
      async plan() {
        return { binary: 'missing-bin', args: [] };
      },
      async beforeSpawn() {},
      async afterExit() {
        return { scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 };
      },
    };

    const code = await runWithAdapter(adapter, emptyArgs([]), {
      spawn() {
        const ee = new EventEmitter() as EventEmitter & { exitCode: number };
        ee.exitCode = 0;
        setImmediate(() => ee.emit('error', new Error('ENOENT')));
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        return ee as any;
      },
    });
    assert.equal(code, 127);
  });
});
