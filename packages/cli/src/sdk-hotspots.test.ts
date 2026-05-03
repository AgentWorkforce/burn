// Drives the SDK's expanded `hotspots()` discriminated union directly so
// non-CLI consumers (MCP, embedders) get coverage of every `kind` the
// function can return: `attribution`, the four narrow `groupBy` shapes
// (`bash` / `bash-verb` / `file` / `subagent`), and `findings`.
//
// The test mounts a tmp `RELAYBURN_HOME`, appends a small fixture turn slice
// (a Read + an Edit + a Bash), and queries through the SDK end-to-end. We
// assert per-`kind` shape (the `kind` discriminator is set; rows are the
// expected per-axis aggregation) rather than exact dollar values, since
// pricing snapshots drift over time.

import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import { __resetIndexCacheForTesting, appendTurns } from '@relayburn/ledger';
import type { TurnRecord } from '@relayburn/reader';

import { hotspots } from '@relayburn/sdk';

const SESSION = 's-sdk-hotspots';

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: SESSION,
    messageId: 'msg-1',
    turnIndex: 0,
    ts: '2026-04-29T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: {
      input: 5000,
      output: 200,
      reasoning: 0,
      cacheRead: 100_000,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    project: '/tmp/project',
    ...overrides,
  };
}

let tmp: string;
const originalHome = process.env['RELAYBURN_HOME'];

beforeEach(async () => {
  tmp = await mkdtemp(path.join(tmpdir(), 'burn-sdk-hotspots-'));
  process.env['RELAYBURN_HOME'] = tmp;
  __resetIndexCacheForTesting();
});

afterEach(async () => {
  if (originalHome !== undefined) process.env['RELAYBURN_HOME'] = originalHome;
  else delete process.env['RELAYBURN_HOME'];
  __resetIndexCacheForTesting();
  await rm(tmp, { recursive: true, force: true });
});

async function seedFixtureTurns(): Promise<void> {
  // Three sequential turns: Read, Edit, Bash. Each carries the coverage
  // flags `attributeHotspots` requires (`hasToolCalls` + `hasToolResultEvents`)
  // so they pass the eligibility gate without needing a sized tool-result
  // sidecar — `attributeHotspots` falls back to even-split.
  await appendTurns([
    fakeTurn({
      messageId: 'msg-1',
      turnIndex: 0,
      ts: '2026-04-29T00:00:00.000Z',
      toolCalls: [
        { id: 'tu-read', name: 'Read', target: '/tmp/foo.ts', argsHash: 'Read:foo' },
      ],
      fidelity: {
        class: 'full',
        granularity: 'per-turn',
        coverage: {
          hasToolCalls: true,
          hasToolResultEvents: true,
          hasSessionRelationships: true,
          hasRawContent: true,
          hasInputTokens: true,
          hasOutputTokens: true,
          hasReasoningTokens: true,
          hasCacheReadTokens: true,
          hasCacheCreateTokens: true,
        },
      },
    }),
    fakeTurn({
      messageId: 'msg-2',
      turnIndex: 1,
      ts: '2026-04-29T00:00:01.000Z',
      toolCalls: [
        { id: 'tu-edit', name: 'Edit', target: '/tmp/foo.ts', argsHash: 'Edit:foo' },
      ],
      fidelity: {
        class: 'full',
        granularity: 'per-turn',
        coverage: {
          hasToolCalls: true,
          hasToolResultEvents: true,
          hasSessionRelationships: true,
          hasRawContent: true,
          hasInputTokens: true,
          hasOutputTokens: true,
          hasReasoningTokens: true,
          hasCacheReadTokens: true,
          hasCacheCreateTokens: true,
        },
      },
    }),
    fakeTurn({
      messageId: 'msg-3',
      turnIndex: 2,
      ts: '2026-04-29T00:00:02.000Z',
      toolCalls: [
        { id: 'tu-bash', name: 'Bash', target: 'ls -la /tmp', argsHash: 'Bash:ls' },
      ],
      fidelity: {
        class: 'full',
        granularity: 'per-turn',
        coverage: {
          hasToolCalls: true,
          hasToolResultEvents: true,
          hasSessionRelationships: true,
          hasRawContent: true,
          hasInputTokens: true,
          hasOutputTokens: true,
          hasReasoningTokens: true,
          hasCacheReadTokens: true,
          hasCacheCreateTokens: true,
        },
      },
    }),
  ]);
}

describe('@relayburn/sdk hotspots() — discriminated union', () => {
  it('default returns kind:"attribution" with every per-axis aggregation populated', async () => {
    await seedFixtureTurns();
    const result = await hotspots({ session: SESSION });
    assert.equal(result.kind, 'attribution');
    if (result.kind !== 'attribution') return;
    assert.equal(result.turnsAnalyzed, 3);
    assert.equal(typeof result.grandTotal, 'number');
    assert.equal(typeof result.attributedTotal, 'number');
    assert.equal(typeof result.unattributedTotal, 'number');
    assert.equal(typeof result.attributionDegraded, 'boolean');
    assert.ok(Array.isArray(result.sessions));
    assert.ok(Array.isArray(result.files));
    assert.ok(Array.isArray(result.bashVerbs));
    assert.ok(Array.isArray(result.bash));
    assert.ok(Array.isArray(result.subagents));
    assert.equal(result.fidelity.refused, false);
    assert.equal(result.fidelity.analyzed, 3);
  });

  it('groupBy:"file" returns kind:"file" with file aggregation rows only', async () => {
    await seedFixtureTurns();
    const result = await hotspots({ session: SESSION, groupBy: 'file' });
    assert.equal(result.kind, 'file');
    if (result.kind !== 'file') return;
    // Read+Edit on /tmp/foo.ts collapses to one row.
    assert.equal(result.rows.length, 1);
    assert.equal(result.rows[0]!.path, '/tmp/foo.ts');
  });

  it('groupBy:"bash" returns kind:"bash" with one row per exact command', async () => {
    await seedFixtureTurns();
    const result = await hotspots({ session: SESSION, groupBy: 'bash' });
    assert.equal(result.kind, 'bash');
    if (result.kind !== 'bash') return;
    assert.equal(result.rows.length, 1);
    assert.equal(result.rows[0]!.command, 'ls -la /tmp');
  });

  it('groupBy:"bash-verb" returns kind:"bash-verb" rolled up by leading verb', async () => {
    await seedFixtureTurns();
    const result = await hotspots({ session: SESSION, groupBy: 'bash-verb' });
    assert.equal(result.kind, 'bash-verb');
    if (result.kind !== 'bash-verb') return;
    assert.equal(result.rows.length, 1);
    assert.equal(result.rows[0]!.verb, 'ls');
    assert.equal(result.rows[0]!.callCount, 1);
  });

  it('groupBy:"subagent" returns kind:"subagent" (empty when no Agent/Task tool calls)', async () => {
    await seedFixtureTurns();
    const result = await hotspots({ session: SESSION, groupBy: 'subagent' });
    assert.equal(result.kind, 'subagent');
    if (result.kind !== 'subagent') return;
    assert.equal(result.rows.length, 0);
  });

  it('rejects an invalid groupBy with a clear error', async () => {
    await assert.rejects(
      // @ts-expect-error — exercising runtime validation
      () => hotspots({ session: SESSION, groupBy: 'nope' }),
      /invalid hotspots groupBy/,
    );
  });

  it('passing patterns ignores groupBy and returns kind:"findings" with a flat findings array', async () => {
    await seedFixtureTurns();
    const result = await hotspots({
      session: SESSION,
      groupBy: 'file', // ignored when patterns is set
      patterns: ['retry-loop'],
    });
    assert.equal(result.kind, 'findings');
    if (result.kind !== 'findings') return;
    assert.ok(Array.isArray(result.findings));
    // Empty fixture has no retry loops; this just asserts the shape.
    assert.equal(result.findings.length, 0);
    assert.ok(result.summary, 'findings shape includes a fidelity summary');
  });

  it('refuses gracefully when every matched turn lacks attribution coverage', async () => {
    // Append a turn that explicitly fails the attribution coverage gate
    // (granularity is per-turn but tool-call flag is false).
    await appendTurns([
      fakeTurn({
        messageId: 'msg-bad',
        turnIndex: 0,
        ts: '2026-04-29T00:00:00.000Z',
        sessionId: 's-sdk-hotspots-bad',
        toolCalls: [],
        fidelity: {
          class: 'partial',
          granularity: 'per-turn',
          coverage: {
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: true,
            hasRawContent: true,
            hasInputTokens: true,
            hasOutputTokens: true,
            hasReasoningTokens: true,
            hasCacheReadTokens: true,
            hasCacheCreateTokens: true,
          },
        },
      }),
    ]);

    const result = await hotspots({ session: 's-sdk-hotspots-bad' });
    assert.equal(result.kind, 'attribution');
    if (result.kind !== 'attribution') return;
    assert.equal(result.refused, true);
    assert.match(result.refusalReason ?? '', /lack tool-call\/tool-result coverage/);
    assert.equal(result.turnsAnalyzed, 0);
    assert.equal(result.fidelity.refused, true);
  });

  it('refusal under groupBy:"bash" returns the narrow shape with rows:[] + refused:true', async () => {
    await appendTurns([
      fakeTurn({
        messageId: 'msg-bad',
        turnIndex: 0,
        ts: '2026-04-29T00:00:00.000Z',
        sessionId: 's-sdk-hotspots-bad-2',
        toolCalls: [],
        fidelity: {
          class: 'partial',
          granularity: 'per-turn',
          coverage: {
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: true,
            hasRawContent: true,
            hasInputTokens: true,
            hasOutputTokens: true,
            hasReasoningTokens: true,
            hasCacheReadTokens: true,
            hasCacheCreateTokens: true,
          },
        },
      }),
    ]);

    const result = await hotspots({ session: 's-sdk-hotspots-bad-2', groupBy: 'bash' });
    assert.equal(result.kind, 'bash');
    if (result.kind !== 'bash') return;
    assert.deepEqual(result.rows, []);
    assert.equal(result.refused, true);
  });
});
