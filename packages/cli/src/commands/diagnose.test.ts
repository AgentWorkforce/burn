import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import {
  __resetIndexCacheForTesting,
  appendContent,
  appendRelationships,
  appendTurns,
} from '@relayburn/ledger';
import type { ContentRecord, SourceKind, TurnRecord } from '@relayburn/reader';

import { runDiagnose } from './diagnose.js';

// Cross-batch counter so every fakeTurn lands a unique (ts, model, usage,
// argsHashPrefix) tuple — otherwise the writer's content-fingerprint dedup
// (see turnContentFingerprint in @relayburn/ledger) would silently drop
// later turns that happen to share defaults with an already-committed one.
let turnCounter = 0;

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  turnCounter++;
  const ss = String(turnCounter % 60).padStart(2, '0');
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    messageId: 'msg-1',
    turnIndex: 0,
    ts: `2026-04-20T00:00:${ss}.000Z`,
    model: 'claude-sonnet-4-6',
    usage: {
      input: 1000 + turnCounter,
      output: 500,
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

function toolCallTurn(opts: {
  source: SourceKind;
  sessionId: string;
  messageId: string;
  toolCallIds: string[];
}): TurnRecord {
  return fakeTurn({
    source: opts.source,
    sessionId: opts.sessionId,
    messageId: opts.messageId,
    toolCalls: opts.toolCallIds.map((id) => ({
      id,
      name: 'Read',
      // Prefix the argsHash with the tool-call id so turnContentFingerprint's
      // first-4-char hash slice is distinct between turns (the writer dedups
      // on (ts, model, usage, firstArgsHashPrefix) across batches).
      argsHash: `${id}-args`,
    })),
  });
}

function toolResultContent(opts: {
  source: SourceKind;
  sessionId: string;
  messageId: string;
  toolUseId: string;
}): ContentRecord {
  return {
    v: 1,
    source: opts.source,
    sessionId: opts.sessionId,
    messageId: opts.messageId,
    ts: '2026-04-20T00:00:01.000Z',
    role: 'tool_result',
    kind: 'tool_result',
    toolResult: {
      toolUseId: opts.toolUseId,
      content: 'ok',
    },
  };
}

interface CapturedOutput {
  stdout: string;
  stderr: string;
  code: number;
}

async function captureDiagnose(
  args: Parameters<typeof runDiagnose>[0],
): Promise<CapturedOutput> {
  const origStdout = process.stdout.write.bind(process.stdout);
  const origStderr = process.stderr.write.bind(process.stderr);
  let stdout = '';
  let stderr = '';
  process.stdout.write = ((chunk: string | Uint8Array): boolean => {
    stdout += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((chunk: string | Uint8Array): boolean => {
    stderr += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stderr.write;
  let code: number;
  try {
    code = await runDiagnose(args);
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
  return { stdout, stderr, code };
}

describe('burn diagnose aggregate (#79)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-diagnose-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-diagnose-relay-'));
    // Point HOME at an empty dir so ingestAll() finds no transcripts to walk —
    // the test fixtures here are pre-seeded directly into the ledger.
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    process.env['RELAYBURN_CONTENT_STORE'] = 'full';
    __resetIndexCacheForTesting();
    turnCounter = 0;
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

  it('reports zero gaps on a ledger where every adapter captures tool_result records', async () => {
    // Claude: 1 session w/ 1 tool call + matching tool_result
    await appendTurns([
      toolCallTurn({
        source: 'claude-code',
        sessionId: 'cl-1',
        messageId: 'cl-1-msg-1',
        toolCallIds: ['tu-cl-1'],
      }),
    ]);
    await appendContent([
      toolResultContent({
        source: 'claude-code',
        sessionId: 'cl-1',
        messageId: 'cl-1-msg-1',
        toolUseId: 'tu-cl-1',
      }),
    ]);
    // Codex: 1 session with no tool calls (still counted in `sessions`).
    await appendTurns([
      fakeTurn({
        source: 'codex',
        sessionId: 'cx-1',
        messageId: 'cx-1-msg-1',
      }),
    ]);

    const out = await captureDiagnose({
      flags: { json: true },
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      adapters: Array<{
        adapter: string;
        sessions: number;
        sessionsWithToolCalls: number;
        gappedSessions: number | null;
        orphanToolCalls: number | null;
        degradedPct: number | null;
      }>;
      contentMode: string;
    };
    assert.equal(parsed.contentMode, 'full');
    const claude = parsed.adapters.find((a) => a.adapter === 'claude');
    const codex = parsed.adapters.find((a) => a.adapter === 'codex');
    assert.ok(claude, 'claude row present');
    assert.equal(claude!.sessions, 1);
    assert.equal(claude!.sessionsWithToolCalls, 1);
    assert.equal(claude!.gappedSessions, 0);
    assert.equal(claude!.orphanToolCalls, 0);
    assert.equal(claude!.degradedPct, 0);
    assert.ok(codex, 'codex row present');
    assert.equal(codex!.sessions, 1);
    assert.equal(codex!.sessionsWithToolCalls, 0);
    assert.equal(codex!.gappedSessions, 0);
  });

  it('reports gapped sessions and degradedPct when adapters emit tool calls without tool_result records', async () => {
    // Codex: two sessions with tool calls but no tool_result content.
    await appendTurns([
      toolCallTurn({
        source: 'codex',
        sessionId: 'cx-A',
        messageId: 'cx-A-msg-1',
        toolCallIds: ['tu-A-1', 'tu-A-2'],
      }),
      toolCallTurn({
        source: 'codex',
        sessionId: 'cx-B',
        messageId: 'cx-B-msg-1',
        toolCallIds: ['tu-B-1'],
      }),
      // Codex no-tool-call session — present in `sessions` but excluded from
      // withToolCalls so degradedPct stays comparable.
      fakeTurn({
        source: 'codex',
        sessionId: 'cx-C',
        messageId: 'cx-C-msg-1',
      }),
    ]);
    // OpenCode: one session that captured content correctly. Demonstrates
    // mixed adapter health in the same ledger.
    await appendTurns([
      toolCallTurn({
        source: 'opencode',
        sessionId: 'oc-1',
        messageId: 'oc-1-msg-1',
        toolCallIds: ['tu-oc-1'],
      }),
    ]);
    await appendContent([
      toolResultContent({
        source: 'opencode',
        sessionId: 'oc-1',
        messageId: 'oc-1-msg-1',
        toolUseId: 'tu-oc-1',
      }),
    ]);

    const out = await captureDiagnose({
      flags: { json: true },
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      adapters: Array<{
        adapter: string;
        sessions: number;
        sessionsWithToolCalls: number;
        gappedSessions: number;
        orphanToolCalls: number;
        degradedPct: number;
      }>;
    };
    const codex = parsed.adapters.find((a) => a.adapter === 'codex');
    assert.ok(codex);
    assert.equal(codex!.sessions, 3);
    assert.equal(codex!.sessionsWithToolCalls, 2);
    assert.equal(codex!.gappedSessions, 2);
    assert.equal(codex!.orphanToolCalls, 3);
    assert.equal(codex!.degradedPct, 100);

    const opencode = parsed.adapters.find((a) => a.adapter === 'opencode');
    assert.ok(opencode);
    assert.equal(opencode!.sessions, 1);
    assert.equal(opencode!.sessionsWithToolCalls, 1);
    assert.equal(opencode!.gappedSessions, 0);
    assert.equal(opencode!.degradedPct, 0);

    // Claude has no sessions in this ledger, so we omit it entirely (as
    // documented in the table-of-zero-rows alternative — see commit body).
    assert.equal(
      parsed.adapters.find((a) => a.adapter === 'claude'),
      undefined,
    );
  });

  it('emits the per-adapter table on the human-readable path', async () => {
    await appendTurns([
      toolCallTurn({
        source: 'codex',
        sessionId: 'cx-X',
        messageId: 'cx-X-msg-1',
        toolCallIds: ['tu-X'],
      }),
    ]);
    const out = await captureDiagnose({
      flags: {},
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    assert.match(out.stdout, /Content-capture gaps by adapter/);
    assert.match(out.stdout, /codex/);
    // Header columns from the issue spec.
    assert.match(out.stdout, /sessions/);
    assert.match(out.stdout, /withToolCalls/);
    assert.match(out.stdout, /gapped/);
    assert.match(out.stdout, /degraded%/);
    // codex row: 1 session, 1 with tool calls, 1 gapped, 100% degraded.
    assert.match(out.stdout, /100\.0%/);
  });

  it('reports spawn-env/native relationship drift with opt-in details', async () => {
    await appendTurns([
      fakeTurn({
        source: 'claude-code',
        sessionId: 'cl-drift',
        messageId: 'cl-drift-1',
      }),
      fakeTurn({
        source: 'claude-code',
        sessionId: 'cl-native-only',
        messageId: 'cl-native-only-1',
      }),
      fakeTurn({
        source: 'claude-code',
        sessionId: 'cl-agree',
        messageId: 'cl-agree-1',
      }),
      fakeTurn({
        source: 'codex',
        sessionId: 'cx-env-only',
        messageId: 'cx-env-only-1',
      }),
      fakeTurn({
        source: 'opencode',
        sessionId: 'oc-agree',
        messageId: 'oc-agree-1',
      }),
    ]);
    await appendRelationships([
      {
        v: 1,
        source: 'spawn-env',
        sessionId: 'cl-drift',
        relatedSessionId: 'ag-parent',
        relationshipType: 'subagent',
      },
      {
        v: 1,
        source: 'native-claude',
        sessionId: 'cl-native-only',
        relatedSessionId: 'cl-parent',
        relationshipType: 'subagent',
        agentId: 'ag-native',
      },
      {
        v: 1,
        source: 'spawn-env',
        sessionId: 'cl-agree',
        relatedSessionId: 'ag-parent',
        relationshipType: 'subagent',
      },
      {
        v: 1,
        source: 'native-claude',
        sessionId: 'cl-agree',
        relatedSessionId: 'cl-parent',
        relationshipType: 'subagent',
        agentId: 'ag-claude-native',
      },
      {
        v: 1,
        source: 'spawn-env',
        sessionId: 'cx-env-only',
        relatedSessionId: 'ag-parent',
        relationshipType: 'subagent',
      },
      {
        v: 1,
        source: 'spawn-env',
        sessionId: 'oc-agree',
        relatedSessionId: 'ag-parent',
        relationshipType: 'subagent',
      },
      {
        v: 1,
        source: 'native-opencode',
        sessionId: 'oc-agree',
        relatedSessionId: 'oc-parent',
        relationshipType: 'subagent',
      },
    ]);

    const out = await captureDiagnose({
      flags: { json: true, 'explain-drift': true },
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      relationshipDrift: {
        sessions: number;
        details: Array<{
          sessionId: string;
          adapter: string;
          reason: string;
          envParentAgentId: string;
        }>;
      };
    };
    assert.equal(parsed.relationshipDrift.sessions, 1);
    assert.deepEqual(parsed.relationshipDrift.details, [
      {
        sessionId: 'cl-drift',
        adapter: 'claude',
        reason: 'spawn-env-without-native',
        envParentAgentId: 'ag-parent',
      },
    ]);

    const human = await captureDiagnose({
      flags: { 'explain-drift': true },
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(human.code, 0);
    assert.match(human.stdout, /Relationship attribution drift/);
    assert.match(human.stdout, /cl-drift/);
  });

  it('omits the gap signal and prints a note when content store is hash-only', async () => {
    process.env['RELAYBURN_CONTENT_STORE'] = 'hash-only';
    await appendTurns([
      toolCallTurn({
        source: 'claude-code',
        sessionId: 'cl-h',
        messageId: 'cl-h-msg-1',
        toolCallIds: ['tu-h'],
      }),
    ]);
    const out = await captureDiagnose({
      flags: { json: true },
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      adapters: Array<{
        adapter: string;
        gappedSessions: number | null;
        orphanToolCalls: number | null;
        degradedPct: number | null;
      }>;
      contentMode: string;
    };
    assert.equal(parsed.contentMode, 'hash-only');
    const claude = parsed.adapters.find((a) => a.adapter === 'claude');
    assert.ok(claude);
    assert.equal(claude!.gappedSessions, null);
    assert.equal(claude!.orphanToolCalls, null);
    assert.equal(claude!.degradedPct, null);

    // Human path includes the explanatory note.
    const human = await captureDiagnose({
      flags: {},
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(human.code, 0);
    assert.match(human.stdout, /content store is hash-only/);
  });

  it('preserves the existing per-session diagnose path when a session id is supplied', async () => {
    // Empty ledger → existing behavior is exit 1 with a "no turns found"
    // message on stderr. We're asserting the per-session branch wasn't
    // accidentally rerouted into the aggregate path.
    const out = await captureDiagnose({
      flags: {},
      tags: {},
      positional: ['does-not-exist'],
      passthrough: [],
    });
    assert.equal(out.code, 1);
    assert.match(out.stderr, /no turns found for session does-not-exist/);
  });

  it('renders an empty-ledger note when there are no adapter sessions at all', async () => {
    const out = await captureDiagnose({
      flags: {},
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    assert.match(out.stdout, /no sessions in ledger/);
  });
});
