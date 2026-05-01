import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import {
  __resetIndexCacheForTesting,
  appendContent,
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
} from '@relayburn/ledger';
import type {
  ContentRecord,
  SessionRelationshipRecord,
  SourceKind,
  ToolResultEventRecord,
  TurnRecord,
} from '@relayburn/reader';

import { runHotspots } from './hotspots.js';
import { runHotspotsSession } from './hotspots-session.js';
import type { ParsedArgs } from '../args.js';

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

function fakeRelationship(
  overrides: Partial<SessionRelationshipRecord> = {},
): SessionRelationshipRecord {
  return {
    v: 1,
    source: 'native-claude',
    sessionId: 's-1',
    relationshipType: 'root',
    ts: '2026-04-20T00:00:01.000Z',
    ...overrides,
  };
}

function fakeToolResultEvent(
  overrides: Partial<ToolResultEventRecord> & {
    sessionId: string;
    toolUseId: string;
    eventIndex: number;
  },
): ToolResultEventRecord {
  const { sessionId, toolUseId, eventIndex, ...rest } = overrides;
  return {
    v: 1,
    source: 'claude-code',
    sessionId,
    toolUseId,
    eventIndex,
    ts: `2026-04-20T00:02:${String(eventIndex).padStart(2, '0')}.000Z`,
    status: 'completed',
    eventSource: 'tool_result',
    contentLength: 2,
    ...rest,
  };
}

interface CapturedOutput {
  stdout: string;
  stderr: string;
  code: number;
}

async function captureHotspots(
  args: ParsedArgs,
  runner: (args: ParsedArgs) => Promise<number> = runHotspotsSession,
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
    code = await runner(args);
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
  return { stdout, stderr, code };
}

describe('burn hotspots --session aggregate (#79)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-hotspots-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-hotspots-relay-'));
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

  it('routes bare hotspots --session through the aggregate gap report', async () => {
    const out = await captureHotspots(
      {
        flags: { session: true, json: true },
        tags: {},
        positional: [],
        passthrough: [],
      },
      runHotspots,
    );
    assert.equal(out.code, 0);
    assert.equal(out.stderr, '');
    const parsed = JSON.parse(out.stdout) as {
      adapters: unknown[];
      contentMode: string;
      relationshipDrift: { sessions: number };
    };
    assert.deepEqual(parsed.adapters, []);
    assert.equal(parsed.contentMode, 'full');
    assert.equal(parsed.relationshipDrift.sessions, 0);
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

    const out = await captureHotspots({
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

    const out = await captureHotspots({
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
    const out = await captureHotspots({
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

    const out = await captureHotspots({
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

    const human = await captureHotspots({
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
    const out = await captureHotspots({
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
    const human = await captureHotspots({
      flags: {},
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(human.code, 0);
    assert.match(human.stdout, /content store is hash-only/);
  });

  it('preserves the existing per-session hotspots session path when a session id is supplied', async () => {
    // Empty ledger → existing behavior is exit 1 with a "no turns found"
    // message on stderr. We're asserting the per-session branch wasn't
    // accidentally rerouted into the aggregate path.
    const out = await captureHotspots({
      flags: {},
      tags: {},
      positional: ['does-not-exist'],
      passthrough: [],
    });
    assert.equal(out.code, 1);
    assert.match(out.stderr, /no turns found for session does-not-exist/);
  });

  it('renders relationship rows and linked tool-result chronology for a session', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'diag-parent',
        messageId: 'diag-parent-1',
      }),
      fakeTurn({
        sessionId: 'diag-child',
        messageId: 'diag-child-1',
        ts: '2026-04-20T00:03:00.000Z',
      }),
    ]);
    await appendRelationships([
      fakeRelationship({
        sessionId: 'diag-parent',
        relationshipType: 'root',
      }),
      fakeRelationship({
        sessionId: 'diag-child',
        relatedSessionId: 'diag-parent',
        relationshipType: 'subagent',
        parentToolUseId: 'tool-spawn',
        agentId: 'agent-review',
        subagentType: 'code-reviewer',
        description: 'inspect execution graph',
      }),
    ]);
    await appendToolResultEvents([
      fakeToolResultEvent({
        sessionId: 'diag-parent',
        toolUseId: 'tool-spawn',
        eventIndex: 0,
        status: 'errored',
        contentLength: 120,
      }),
      fakeToolResultEvent({
        sessionId: 'diag-parent',
        toolUseId: 'tool-spawn',
        eventIndex: 1,
        status: 'errored',
        contentLength: 128,
      }),
      fakeToolResultEvent({
        sessionId: 'diag-parent',
        toolUseId: 'tool-spawn',
        eventIndex: 2,
        status: 'completed',
        contentLength: 80,
      }),
      fakeToolResultEvent({
        sessionId: 'diag-child',
        toolUseId: 'child-read',
        eventIndex: 0,
        status: 'errored',
        contentLength: 64,
      }),
    ]);

    const out = await captureHotspots({
      flags: {},
      tags: {},
      positional: ['diag-parent'],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    assert.match(out.stdout, /tool results: 1 of 2 tool calls errored across 2 linked sessions/);
    assert.match(out.stdout, /Session relationships/);
    assert.match(out.stdout, /diag-child/);
    assert.match(out.stdout, /subagent/);
    assert.match(out.stdout, /code-reviewer/);
    assert.match(out.stdout, /tool-spawn/);
    assert.match(out.stdout, /inspect execution graph/);
    assert.match(out.stdout, /Tool result chronology/);
    assert.match(out.stdout, /errored \(2x\)/);
    assert.match(out.stdout, /child-read/);
  });

  it('adds graph arrays to hotspots --session --json', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'diag-json',
        messageId: 'diag-json-1',
      }),
    ]);
    await appendRelationships([
      fakeRelationship({
        sessionId: 'diag-json',
        relationshipType: 'root',
      }),
    ]);
    await appendToolResultEvents([
      fakeToolResultEvent({
        sessionId: 'diag-json',
        toolUseId: 'json-read',
        eventIndex: 0,
        status: 'completed',
        contentLength: 42,
      }),
    ]);

    const out = await captureHotspots({
      flags: { json: true },
      tags: {},
      positional: ['diag-json'],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    const parsed = JSON.parse(out.stdout) as {
      relationships: SessionRelationshipRecord[];
      toolResultEvents: ToolResultEventRecord[];
      toolResultStatusBySession: Array<{
        sessionId: string;
        toolCalls: number;
        completed: number;
      }>;
    };
    assert.equal(parsed.relationships.length, 1);
    assert.equal(parsed.relationships[0]!.relationshipType, 'root');
    assert.equal(parsed.toolResultEvents.length, 1);
    assert.equal(parsed.toolResultEvents[0]!.toolUseId, 'json-read');
    assert.deepEqual(parsed.toolResultStatusBySession, [
      {
        sessionId: 'diag-json',
        toolCalls: 1,
        completed: 1,
        errored: 0,
        cancelled: 0,
        unknown: 0,
      },
    ]);
  });

  it('adds Bash verb rollups to per-session JSON and human reports', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'bash-session',
        messageId: 'bash-session-1',
        turnIndex: 0,
        toolCalls: [
          { id: 'bash-a', name: 'Bash', target: 'git diff src/a.ts', argsHash: 'git:diff:a' },
        ],
      }),
      fakeTurn({
        sessionId: 'bash-session',
        messageId: 'bash-session-2',
        turnIndex: 1,
        usage: { input: 1000, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      fakeTurn({
        sessionId: 'bash-session',
        messageId: 'bash-session-3',
        turnIndex: 2,
        toolCalls: [
          { id: 'bash-b', name: 'Bash', target: 'git diff src/b.ts', argsHash: 'git:diff:b' },
        ],
      }),
      fakeTurn({
        sessionId: 'bash-session',
        messageId: 'bash-session-4',
        turnIndex: 3,
        usage: { input: 1000, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ]);
    await appendContent([
      {
        ...toolResultContent({
          source: 'claude-code',
          sessionId: 'bash-session',
          messageId: 'bash-session-result-a',
          toolUseId: 'bash-a',
        }),
        toolResult: { toolUseId: 'bash-a', content: 'a'.repeat(4000) },
      },
      {
        ...toolResultContent({
          source: 'claude-code',
          sessionId: 'bash-session',
          messageId: 'bash-session-result-b',
          toolUseId: 'bash-b',
        }),
        toolResult: { toolUseId: 'bash-b', content: 'b'.repeat(4000) },
      },
    ]);

    const json = await captureHotspots({
      flags: { json: true },
      tags: {},
      positional: ['bash-session'],
      passthrough: [],
    });
    assert.equal(json.code, 0);
    const parsed = JSON.parse(json.stdout) as {
      topBashVerbs: Array<{ verb: string; callCount: number; distinctCommands: number }>;
      topBashes: unknown[];
    };
    assert.equal(parsed.topBashVerbs[0]!.verb, 'git diff');
    assert.equal(parsed.topBashVerbs[0]!.callCount, 2);
    assert.equal(parsed.topBashVerbs[0]!.distinctCommands, 2);
    assert.equal(parsed.topBashes.length, 2);

    const human = await captureHotspots({
      flags: {},
      tags: {},
      positional: ['bash-session'],
      passthrough: [],
    });
    assert.equal(human.code, 0);
    const verbIndex = human.stdout.indexOf('Top Bash verbs by cost');
    const exactIndex = human.stdout.indexOf('Top exact Bash commands by cost');
    assert.ok(verbIndex >= 0, 'verb heading present');
    assert.ok(exactIndex > verbIndex, 'exact-command heading follows verb heading');
    assert.match(human.stdout, /git diff/);
  });

  it('keeps the human per-session output unchanged when no graph rows exist', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'diag-empty-graph',
        messageId: 'diag-empty-graph-1',
      }),
    ]);

    const out = await captureHotspots({
      flags: {},
      tags: {},
      positional: ['diag-empty-graph'],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    assert.doesNotMatch(out.stdout, /Session relationships/);
    assert.doesNotMatch(out.stdout, /Tool result chronology/);
    assert.doesNotMatch(out.stdout, /tool results:/);
  });

  it('renders an empty-ledger note when there are no adapter sessions at all', async () => {
    const out = await captureHotspots({
      flags: {},
      tags: {},
      positional: [],
      passthrough: [],
    });
    assert.equal(out.code, 0);
    assert.match(out.stdout, /no sessions in ledger/);
  });
});
