import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import {
  __resetIndexCacheForTesting,
  appendUserTurns,
} from '@relayburn/ledger';
import type { UserTurnRecord } from '@relayburn/reader';

import { bulkUserTurnsBySession } from './hotspots.js';

function fakeUserTurn(overrides: Partial<UserTurnRecord> = {}): UserTurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-x',
    userUuid: 'uu-1',
    ts: '2026-04-20T00:00:00.000Z',
    blocks: [],
    ...overrides,
  };
}

describe('bulkUserTurnsBySession — query narrowing (#214 follow-up)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-bulk-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-bulk-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('forwards q.since to queryUserTurns so historical user turns are not buffered', async () => {
    // Three sessions, each with one ancient user turn (2024) and one recent
    // user turn (2026-04-20). With `since: 2026-04-01`, only the recent turns
    // should appear in the result map — the ancient ones must be filtered
    // during streaming, not after, otherwise long historical ledgers blow
    // memory on a small recent window.
    const sessions = ['s-1', 's-2', 's-3'];
    const records: UserTurnRecord[] = [];
    for (const sessionId of sessions) {
      records.push(
        fakeUserTurn({
          sessionId,
          userUuid: `${sessionId}-old`,
          ts: '2024-01-01T00:00:00.000Z',
        }),
        fakeUserTurn({
          sessionId,
          userUuid: `${sessionId}-new`,
          ts: '2026-04-20T00:00:00.000Z',
        }),
      );
    }
    await appendUserTurns(records);

    const out = await bulkUserTurnsBySession(new Set(sessions), {
      since: '2026-04-01T00:00:00.000Z',
    });

    assert.equal(out.size, 3);
    for (const sessionId of sessions) {
      const got = out.get(sessionId);
      assert.ok(got, `expected results for ${sessionId}`);
      assert.equal(got.length, 1, `expected only the recent turn for ${sessionId}`);
      assert.equal(got[0]!.userUuid, `${sessionId}-new`);
    }
  });

  it('drops user turns whose sessionId is outside the requested set', async () => {
    await appendUserTurns([
      fakeUserTurn({ sessionId: 's-keep', userUuid: 'k1' }),
      fakeUserTurn({ sessionId: 's-drop', userUuid: 'd1' }),
    ]);

    const out = await bulkUserTurnsBySession(new Set(['s-keep']));

    assert.equal(out.size, 1);
    assert.ok(out.has('s-keep'));
    assert.equal(out.has('s-drop'), false);
  });

  it('returns an empty map without touching the ledger when sessionIds is empty', async () => {
    // No appendUserTurns call — the ledger file may not exist. Helper must
    // short-circuit before any I/O.
    const out = await bulkUserTurnsBySession(new Set());
    assert.equal(out.size, 0);
  });
});
