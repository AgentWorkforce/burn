import { strict as assert } from 'node:assert';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import {
  __resetIndexCacheForTesting,
  appendTurns,
  archivePath,
  buildArchive,
} from '@relayburn/ledger';

import { createSessionCostTool, type SessionCostResult } from './session-cost.js';

// Verifies the archive-backed default `queryTurns` path that issue #97 wires
// onto MCP tool handlers — including the transparent fallback to `queryAll`
// when the archive cannot be opened or queried.

// Minimal `TurnRecord` shape inlined here so MCP doesn't need to depend on
// `@relayburn/reader` (which is being retired as a published package at the
// 2.0 cutover). Captures only the fields these archive-backed integration
// tests construct and pass through to `appendTurns`; the ledger writer
// preserves all fields verbatim and the SDK consumes them through the same
// JSONL → SQLite path.
interface TestTurnRecord {
  v: 1;
  source: 'claude-code';
  sessionId: string;
  messageId: string;
  turnIndex: number;
  ts: string;
  model: string;
  usage: {
    input: number;
    output: number;
    reasoning: number;
    cacheRead: number;
    cacheCreate5m: number;
    cacheCreate1h: number;
  };
  toolCalls: never[];
  project: string;
}

function fakeTurn(overrides: Partial<TestTurnRecord> = {}): TestTurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-cost',
    messageId: 'm-1',
    turnIndex: 0,
    ts: '2026-04-24T10:00:00.000Z',
    model: 'claude-sonnet-4-5',
    usage: {
      input: 1_000_000,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    project: '/tmp/project',
    ...overrides,
  };
}

describe('MCP tool handlers backed by archive (issue #97)', () => {
  let tmpDir: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-mcp-archive-'));
  });

  beforeEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-mcp-archive-'));
    process.env['RELAYBURN_HOME'] = tmpDir;
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalHome !== undefined) {
      process.env['RELAYBURN_HOME'] = originalHome;
    } else {
      delete process.env['RELAYBURN_HOME'];
    }
    await rm(tmpDir, { recursive: true, force: true });
  });

  describe('burn__sessionCost', () => {
    it('returns the same total whether the data is read from archive or ledger', async () => {
      // Same fixture, identical pricing — archive-backed query should be
      // bit-equivalent to the ledger-walk implementation it replaces.
      await appendTurns([
        fakeTurn(),
        fakeTurn({
          messageId: 'm-2',
          turnIndex: 1,
          usage: {
            input: 0,
            output: 1_000_000,
            reasoning: 0,
            cacheRead: 0,
            cacheCreate5m: 0,
            cacheCreate1h: 0,
          },
        }),
      ]);
      await buildArchive();

      const tool = createSessionCostTool({
        defaultSessionId: 's-cost',
      });
      const result = (await tool.handler({})) as SessionCostResult;
      assert.equal(result.sessionId, 's-cost');
      assert.equal(result.turnCount, 2);
      assert.equal(result.totalTokens, 2_000_000);
      // 1M input @ $3/M + 1M output @ $15/M = $18.
      assert.equal(result.totalUSD, 18);
      assert.deepEqual(result.models, ['claude-sonnet-4-5']);
    });

    it('reflects ledger turns appended after the initial archive build', async () => {
      // Hooks append to the JSONL ledger throughout a session but the archive
      // is only built on demand. The default queryTurns lambda runs an
      // incremental buildArchive before each query so a tool call after a
      // hook fires reflects the new turns (Devin review on #97).
      await appendTurns([fakeTurn()]);
      await buildArchive();

      // Simulate a hook firing mid-session: a new turn is appended after the
      // initial build but no explicit rebuild has been triggered.
      await appendTurns([
        fakeTurn({
          messageId: 'm-2',
          turnIndex: 1,
          ts: '2026-04-24T10:05:00.000Z',
          usage: {
            input: 0,
            output: 1_000_000,
            reasoning: 0,
            cacheRead: 0,
            cacheCreate5m: 0,
            cacheCreate1h: 0,
          },
        }),
      ]);

      const tool = createSessionCostTool({
        defaultSessionId: 's-cost',
      });
      const result = (await tool.handler({})) as SessionCostResult;
      assert.equal(result.turnCount, 2);
      assert.equal(result.totalTokens, 2_000_000);
      // 1M input @ $3/M + 1M output @ $15/M = $18.
      assert.equal(result.totalUSD, 18);
    });

    it('falls back to queryAll and logs when the archive cannot be opened', async () => {
      await appendTurns([fakeTurn()]);
      // Don't build the archive. Then corrupt the archive file so
      // openArchive() throws once it tries to read the SQLite header. The
      // tool should swallow the error, route to queryAll, and still return
      // the same numbers.
      await writeFile(archivePath(), 'this is not a sqlite database', 'utf8');

      const logs: string[] = [];
      const tool = createSessionCostTool({
        defaultSessionId: 's-cost',
        onLog: (m) => logs.push(m),
      });
      const result = (await tool.handler({})) as SessionCostResult;
      assert.equal(result.turnCount, 1);
      assert.equal(result.totalTokens, 1_000_000);
      // 1M input @ $3/M = $3.
      assert.equal(result.totalUSD, 3);
      assert.ok(
        logs.some((m) => /archive query failed/.test(m)),
        `expected an archive-fallback log line, got: ${JSON.stringify(logs)}`,
      );
    });
  });
});
