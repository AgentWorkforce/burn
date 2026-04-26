import { strict as assert } from 'node:assert';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import { queryAll } from '@relayburn/ledger';

import { runWatchTick, startWatchLoop } from './watch.js';

describe('burn watch', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-watch-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-watch-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    delete process.env['RELAYBURN_CONTENT_STORE'];
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('single tick ingests newly committed codex turns through the shared cursor path', async () => {
    await writeCodexSession(
      tmpHome,
      'watch-rollout',
      codexCommittedSession('sess_watch_codex', 'turn_watch_codex', '/tmp/project'),
    );

    const report = await runWatchTick();

    assert.equal(report.ingestedSessions, 1);
    assert.equal(report.appendedTurns, 1);
    const turns = await queryAll({ sessionId: 'sess_watch_codex' });
    assert.equal(turns.length, 1);
    assert.equal(turns[0]!.source, 'codex');
  });

  it('serializes overlapping ticks in the foreground loop', async () => {
    let calls = 0;
    let release: (() => void) | undefined;
    const unblock = new Promise<void>((resolve) => {
      release = resolve;
    });
    const controller = startWatchLoop({
      intervalMs: 1_000_000,
      immediate: false,
      ingest: async () => {
        calls++;
        await unblock;
        return { scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 };
      },
    });

    const first = controller.tick();
    const second = controller.tick();
    assert.equal(calls, 1);
    release!();
    await Promise.all([first, second]);
    await controller.stop();
    assert.equal(calls, 1);
  });
});

async function writeCodexSession(home: string, name: string, body: string): Promise<string> {
  const dir = path.join(home, '.codex', 'sessions', '2026', '04', '24');
  await mkdir(dir, { recursive: true });
  const file = path.join(dir, `${name}.jsonl`);
  await writeFile(file, body, 'utf8');
  return file;
}

function codexCommittedSession(sessionId: string, turnId: string, cwd: string): string {
  const lines = [
    {
      timestamp: '2026-04-20T01:00:00.000Z',
      type: 'session_meta',
      payload: { id: sessionId, cwd, timestamp: '2026-04-20T01:00:00.000Z' },
    },
    {
      timestamp: '2026-04-20T01:00:00.100Z',
      type: 'turn_context',
      payload: { turn_id: turnId, cwd, model: 'gpt-5.3-codex' },
    },
    {
      timestamp: '2026-04-20T01:00:00.200Z',
      type: 'event_msg',
      payload: { type: 'task_started', turn_id: turnId },
    },
    {
      timestamp: '2026-04-20T01:00:01.000Z',
      type: 'event_msg',
      payload: {
        type: 'token_count',
        info: {
          total_token_usage: {
            input_tokens: 12,
            cached_input_tokens: 2,
            output_tokens: 4,
            reasoning_output_tokens: 1,
          },
        },
      },
    },
    {
      timestamp: '2026-04-20T01:00:02.000Z',
      type: 'event_msg',
      payload: { type: 'task_complete', turn_id: turnId },
    },
  ];
  return lines.map((line) => JSON.stringify(line)).join('\n') + '\n';
}
