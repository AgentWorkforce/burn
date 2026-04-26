import { strict as assert } from 'node:assert';
import { mkdir, mkdtemp, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import type { ContentRecord, TurnRecord } from '@relayburn/reader';
import {
  ledgerPath,
  queryRelationships,
  queryToolResultEvents,
  queryUserTurns,
} from '@relayburn/ledger';

import {
  countToolCallGaps,
  ingestClaudeProjects,
  ingestCodexSessions,
  ingestOpencodeSessions,
  resetIngestGapWarnings,
  setIngestGapWriter,
} from './ingest.js';

describe('countToolCallGaps', () => {
  it('flags a session with tool calls but zero tool_result records', () => {
    const turns: TurnRecord[] = [
      makeTurn({ messageId: 'm1', toolCallCount: 2 }),
      makeTurn({ messageId: 'm2', toolCallCount: 1 }),
    ];
    const content: ContentRecord[] = [
      // text + tool_use only — no tool_result
      makeContent({ messageId: 'm1', kind: 'text', role: 'assistant' }),
      makeContent({ messageId: 'm1', kind: 'tool_use', role: 'assistant' }),
    ];
    const result = countToolCallGaps(turns, content);
    assert.equal(result.sessionAffected, true);
    assert.equal(result.orphanToolCalls, 3);
  });

  it('does not flag a session with no tool calls (chat-only)', () => {
    const turns: TurnRecord[] = [makeTurn({ messageId: 'm1', toolCallCount: 0 })];
    const content: ContentRecord[] = [
      makeContent({ messageId: 'm1', kind: 'text', role: 'user' }),
      makeContent({ messageId: 'm1', kind: 'text', role: 'assistant' }),
    ];
    const result = countToolCallGaps(turns, content);
    assert.equal(result.sessionAffected, false);
    assert.equal(result.orphanToolCalls, 0);
  });

  it('does not flag a session that has tool_result records', () => {
    const turns: TurnRecord[] = [makeTurn({ messageId: 'm1', toolCallCount: 1 })];
    const content: ContentRecord[] = [
      makeContent({ messageId: 'm1', kind: 'tool_use', role: 'assistant' }),
      makeContent({ messageId: 'm1', kind: 'tool_result', role: 'tool_result' }),
    ];
    const result = countToolCallGaps(turns, content);
    assert.equal(result.sessionAffected, false);
  });
});

describe('ingest gap warning (codex parser-gap scenario)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-gap-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-gap-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    delete process.env['RELAYBURN_CONTENT_STORE'];
    resetIngestGapWarnings();
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    resetIngestGapWarnings();
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('emits one warning when a codex session has tool calls but no tool_result records', async () => {
    await writeCodexSession(tmpHome, 'rollout-1', codexSessionWithToolCallNoOutput());
    const captured: string[] = [];
    const restore = setIngestGapWriter((msg) => {
      captured.push(msg);
    });
    try {
      await ingestCodexSessions();
    } finally {
      setIngestGapWriter(restore);
    }

    assert.equal(captured.length, 1, 'one warning emitted');
    const msg = captured[0]!;
    assert.match(msg, /codex parser produced 0 tool_result records/);
    assert.match(msg, /1 session/);
    // 2 function_calls in the fixture (exec + patch).
    assert.match(msg, /2 tool calls/);
    assert.match(msg, /See #33/);
    assert.match(msg, /even-split attribution/);
  });

  it('does not warn when contentMode is hash-only', async () => {
    await writeCodexSession(tmpHome, 'rollout-2', codexSessionWithToolCallNoOutput());
    process.env['RELAYBURN_CONTENT_STORE'] = 'hash-only';
    const captured: string[] = [];
    const restore = setIngestGapWriter((msg) => {
      captured.push(msg);
    });
    try {
      await ingestCodexSessions();
    } finally {
      setIngestGapWriter(restore);
    }
    assert.equal(captured.length, 0, 'no warning under hash-only mode');
  });

  it('does not warn when contentMode is off', async () => {
    await writeCodexSession(tmpHome, 'rollout-3', codexSessionWithToolCallNoOutput());
    process.env['RELAYBURN_CONTENT_STORE'] = 'off';
    const captured: string[] = [];
    const restore = setIngestGapWriter((msg) => {
      captured.push(msg);
    });
    try {
      await ingestCodexSessions();
    } finally {
      setIngestGapWriter(restore);
    }
    assert.equal(captured.length, 0, 'no warning under off mode');
  });

  it('does not warn when sessions have zero tool calls', async () => {
    await writeCodexSession(tmpHome, 'rollout-chat-only', codexChatOnlySession());
    const captured: string[] = [];
    const restore = setIngestGapWriter((msg) => {
      captured.push(msg);
    });
    try {
      await ingestCodexSessions();
    } finally {
      setIngestGapWriter(restore);
    }
    assert.equal(captured.length, 0, 'no warning when there are no tool calls');
  });

  it('suppresses repeat warnings on subsequent ingest calls in the same process', async () => {
    await writeCodexSession(tmpHome, 'rollout-suppress', codexSessionWithToolCallNoOutput());
    const captured: string[] = [];
    const restore = setIngestGapWriter((msg) => {
      captured.push(msg);
    });
    try {
      await ingestCodexSessions();
      // Second invocation: cursor is at EOF, no new affected sessions, so the
      // suppression gate keeps stderr quiet.
      await ingestCodexSessions();
      await ingestCodexSessions();
    } finally {
      setIngestGapWriter(restore);
    }
    assert.equal(captured.length, 1, 'second/third ingest stays silent');
  });

  it('persists Codex graph records emitted by passive ingest', async () => {
    await writeCodexSession(tmpHome, 'rollout-graph', codexSessionWithToolOutput());
    await ingestCodexSessions();

    const relationships = await queryRelationships({ source: 'codex' });
    assert.equal(relationships.length, 1);
    assert.equal(relationships[0]!.relationshipType, 'root');
    assert.equal(relationships[0]!.sessionId, 'sess_graph_1');

    const events = await queryToolResultEvents({ source: 'codex' });
    assert.equal(events.length, 1);
    assert.equal(events[0]!.toolUseId, 'call_graph_1');
    assert.equal(events[0]!.status, 'completed');
    assert.equal(typeof events[0]!.contentHash, 'string');
  });

  it('persists OpenCode graph records emitted by passive ingest', async () => {
    await writeOpencodeSession(tmpHome);
    await ingestOpencodeSessions();

    const relationships = await queryRelationships({ source: 'opencode' });
    assert.equal(relationships.length, 1);
    assert.equal(relationships[0]!.relationshipType, 'root');
    assert.equal(relationships[0]!.sessionId, 'ses_oc_graph');

    const events = await queryToolResultEvents({ source: 'opencode' });
    assert.equal(events.length, 1);
    assert.equal(events[0]!.toolUseId, 'call_oc_graph');
    assert.equal(events[0]!.status, 'errored');
    assert.equal(events[0]!.isError, true);
    assert.equal(typeof events[0]!.contentHash, 'string');
  });
});

describe('ingest forwards UserTurnRecord into the ledger (#94)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-ut-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-ut-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    resetIngestGapWarnings();
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    resetIngestGapWarnings();
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('writes user-turn lines for a Claude session and dedupes on re-ingest', async () => {
    await writeClaudeSession(tmpHome, 'sess-ut-1', claudeSessionWithUserTurn());

    await ingestClaudeProjects();
    const first = await queryUserTurns();
    assert.equal(first.length, 1, 'one user-turn line written');
    assert.equal(first[0]!.userUuid, 'u-user-1');
    assert.equal(first[0]!.blocks.length, 1);
    assert.equal(first[0]!.blocks[0]!.kind, 'text');

    // Re-ingest must be a no-op for already-persisted user turns.
    const sizeBefore = (await stat(ledgerPath())).size;
    await ingestClaudeProjects();
    const sizeAfter = (await stat(ledgerPath())).size;
    assert.equal(sizeAfter, sizeBefore, 'ledger must not grow on idempotent re-ingest');
    const second = await queryUserTurns();
    assert.equal(second.length, 1, 'still one user-turn line after re-ingest');
  });
});

// ---------- helpers ----------

async function writeClaudeSession(home: string, sessionId: string, body: string): Promise<void> {
  // Real claude layout is ~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl.
  const encoded = '-tmp-project'; // matches '/tmp/project' encoded as '-' joins
  const dir = path.join(home, '.claude', 'projects', encoded);
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, `${sessionId}.jsonl`), body, 'utf8');
}

// One user line + one assistant line — the parser emits a single
// UserTurnRecord with a `text` block carrying the user's prompt.
function claudeSessionWithUserTurn(): string {
  const sid = '22222222-2222-2222-2222-222222222222';
  const lines = [
    {
      type: 'permission-mode',
      permissionMode: 'default',
      sessionId: sid,
    },
    {
      parentUuid: null,
      isSidechain: false,
      promptId: 'p-1',
      type: 'user',
      message: { role: 'user', content: 'please fix the build' },
      uuid: 'u-user-1',
      timestamp: '2026-04-20T00:00:00.000Z',
      cwd: '/tmp/project',
      sessionId: sid,
      version: '2.1.96',
    },
    {
      parentUuid: 'u-user-1',
      isSidechain: false,
      message: {
        model: 'claude-sonnet-4-6',
        id: 'msg_ut_1',
        type: 'message',
        role: 'assistant',
        content: [{ type: 'text', text: 'Hello!' }],
        stop_reason: 'end_turn',
        stop_sequence: null,
        usage: {
          input_tokens: 10,
          cache_creation_input_tokens: 100,
          cache_read_input_tokens: 500,
          cache_creation: { ephemeral_5m_input_tokens: 80, ephemeral_1h_input_tokens: 20 },
          output_tokens: 5,
          service_tier: 'standard',
        },
      },
      requestId: 'req_1',
      type: 'assistant',
      uuid: 'u-asst-1',
      timestamp: '2026-04-20T00:00:01.000Z',
      cwd: '/tmp/project',
      sessionId: sid,
      version: '2.1.96',
    },
  ];
  return lines.map((l) => JSON.stringify(l)).join('\n') + '\n';
}

function makeTurn(opts: { messageId: string; toolCallCount: number }): TurnRecord {
  return {
    v: 1,
    source: 'codex',
    sessionId: 'sess_test',
    messageId: opts.messageId,
    turnIndex: 0,
    ts: '2026-04-22T00:00:00.000Z',
    model: 'gpt-5',
    usage: {
      input: 1,
      output: 1,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: Array.from({ length: opts.toolCallCount }, (_, i) => ({
      id: `${opts.messageId}-tc-${i}`,
      name: 'exec_command',
      argsHash: 'h',
    })),
  };
}

function makeContent(opts: {
  messageId: string;
  kind: ContentRecord['kind'];
  role: ContentRecord['role'];
}): ContentRecord {
  return {
    v: 1,
    source: 'codex',
    sessionId: 'sess_test',
    messageId: opts.messageId,
    ts: '2026-04-22T00:00:00.000Z',
    role: opts.role,
    kind: opts.kind,
    text: 'x',
  };
}

async function writeCodexSession(home: string, name: string, body: string): Promise<void> {
  // Real codex layout is ~/.codex/sessions/YYYY/MM/DD/<rollout>.jsonl. The
  // walker is recursive, so we can use any nested layout under sessions/.
  const dir = path.join(home, '.codex', 'sessions', '2026', '04', '24');
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, `${name}.jsonl`), body, 'utf8');
}

// A codex session with a committed turn (task_started → task_complete) that
// records two function_call response_items but never emits a
// function_call_output. The parser's contentMode='full' path produces zero
// `tool_result` ContentRecords for this shape — exactly the silent-gap shape
// #59 is meant to surface.
function codexSessionWithToolCallNoOutput(): string {
  const lines = [
    {
      timestamp: '2026-04-20T01:00:00.000Z',
      type: 'session_meta',
      payload: { id: 'sess_gap_1', cwd: '/tmp/project', timestamp: '2026-04-20T01:00:00.000Z' },
    },
    {
      timestamp: '2026-04-20T01:00:00.100Z',
      type: 'turn_context',
      payload: { turn_id: 'turn_gap_1', cwd: '/tmp/project', model: 'gpt-5.3-codex' },
    },
    {
      timestamp: '2026-04-20T01:00:00.200Z',
      type: 'event_msg',
      payload: { type: 'task_started', turn_id: 'turn_gap_1' },
    },
    {
      timestamp: '2026-04-20T01:00:01.000Z',
      type: 'response_item',
      payload: {
        type: 'function_call',
        name: 'exec_command',
        arguments: '{"cmd":"git status"}',
        call_id: 'call_exec_1',
      },
    },
    {
      timestamp: '2026-04-20T01:00:02.000Z',
      type: 'response_item',
      payload: {
        type: 'function_call',
        name: 'exec_command',
        arguments: '{"cmd":"ls"}',
        call_id: 'call_exec_2',
      },
    },
    {
      timestamp: '2026-04-20T01:00:04.000Z',
      type: 'event_msg',
      payload: {
        type: 'token_count',
        info: {
          total_token_usage: {
            input_tokens: 1000,
            cached_input_tokens: 0,
            output_tokens: 100,
            reasoning_output_tokens: 0,
            total_tokens: 1100,
          },
        },
      },
    },
    {
      timestamp: '2026-04-20T01:00:04.100Z',
      type: 'event_msg',
      payload: { type: 'task_complete', turn_id: 'turn_gap_1' },
    },
  ];
  return lines.map((l) => JSON.stringify(l)).join('\n') + '\n';
}

function codexChatOnlySession(): string {
  const lines = [
    {
      timestamp: '2026-04-20T01:00:00.000Z',
      type: 'session_meta',
      payload: { id: 'sess_chat_1', cwd: '/tmp/project', timestamp: '2026-04-20T01:00:00.000Z' },
    },
    {
      timestamp: '2026-04-20T01:00:00.100Z',
      type: 'turn_context',
      payload: { turn_id: 'turn_chat_1', cwd: '/tmp/project', model: 'gpt-5.3-codex' },
    },
    {
      timestamp: '2026-04-20T01:00:00.200Z',
      type: 'event_msg',
      payload: { type: 'task_started', turn_id: 'turn_chat_1' },
    },
    {
      timestamp: '2026-04-20T01:00:01.000Z',
      type: 'response_item',
      payload: {
        type: 'message',
        role: 'user',
        content: [{ type: 'input_text', text: 'hi' }],
      },
    },
    {
      timestamp: '2026-04-20T01:00:02.000Z',
      type: 'response_item',
      payload: {
        type: 'message',
        role: 'assistant',
        content: [{ type: 'output_text', text: 'hello' }],
      },
    },
    {
      timestamp: '2026-04-20T01:00:04.000Z',
      type: 'event_msg',
      payload: {
        type: 'token_count',
        info: {
          total_token_usage: {
            input_tokens: 100,
            cached_input_tokens: 0,
            output_tokens: 10,
            reasoning_output_tokens: 0,
            total_tokens: 110,
          },
        },
      },
    },
    {
      timestamp: '2026-04-20T01:00:04.100Z',
      type: 'event_msg',
      payload: { type: 'task_complete', turn_id: 'turn_chat_1' },
    },
  ];
  return lines.map((l) => JSON.stringify(l)).join('\n') + '\n';
}

function codexSessionWithToolOutput(): string {
  const lines = [
    {
      timestamp: '2026-04-20T01:00:00.000Z',
      type: 'session_meta',
      payload: { id: 'sess_graph_1', cwd: '/tmp/project', timestamp: '2026-04-20T01:00:00.000Z' },
    },
    {
      timestamp: '2026-04-20T01:00:00.100Z',
      type: 'turn_context',
      payload: { turn_id: 'turn_graph_1', cwd: '/tmp/project', model: 'gpt-5.3-codex' },
    },
    {
      timestamp: '2026-04-20T01:00:00.200Z',
      type: 'event_msg',
      payload: { type: 'task_started', turn_id: 'turn_graph_1' },
    },
    {
      timestamp: '2026-04-20T01:00:01.000Z',
      type: 'response_item',
      payload: {
        type: 'function_call',
        name: 'exec_command',
        arguments: '{"cmd":"git status"}',
        call_id: 'call_graph_1',
      },
    },
    {
      timestamp: '2026-04-20T01:00:01.100Z',
      type: 'response_item',
      payload: {
        type: 'function_call_output',
        call_id: 'call_graph_1',
        output: 'clean',
      },
    },
    {
      timestamp: '2026-04-20T01:00:01.200Z',
      type: 'event_msg',
      payload: { type: 'exec_command_end', call_id: 'call_graph_1', turn_id: 'turn_graph_1', exit_code: 0 },
    },
    {
      timestamp: '2026-04-20T01:00:02.000Z',
      type: 'event_msg',
      payload: {
        type: 'token_count',
        info: { total_token_usage: { input_tokens: 100, cached_input_tokens: 0, output_tokens: 10 } },
      },
    },
    {
      timestamp: '2026-04-20T01:00:02.100Z',
      type: 'event_msg',
      payload: { type: 'task_complete', turn_id: 'turn_graph_1' },
    },
  ];
  return lines.map((l) => JSON.stringify(l)).join('\n') + '\n';
}

async function writeOpencodeSession(home: string): Promise<void> {
  const storage = path.join(home, '.local', 'share', 'opencode', 'storage');
  const sessionDir = path.join(storage, 'session', 'global');
  const messageDir = path.join(storage, 'message', 'ses_oc_graph');
  const partDir = path.join(storage, 'part', 'msg_oc_graph_asst');
  await mkdir(sessionDir, { recursive: true });
  await mkdir(messageDir, { recursive: true });
  await mkdir(partDir, { recursive: true });
  await writeFile(
    path.join(sessionDir, 'ses_oc_graph.json'),
    JSON.stringify({
      id: 'ses_oc_graph',
      version: '1.0.0',
      directory: '/tmp/project',
      time: { created: 1_776_988_000_000, updated: 1_776_988_001_000 },
    }),
    'utf8',
  );
  await writeFile(
    path.join(messageDir, 'msg_oc_graph_asst.json'),
    JSON.stringify({
      id: 'msg_oc_graph_asst',
      sessionID: 'ses_oc_graph',
      role: 'assistant',
      providerID: 'anthropic',
      modelID: 'claude-sonnet-4-5',
      time: { created: 1_776_988_001_000 },
      path: { cwd: '/tmp/project' },
      tokens: { input: 10, output: 5, cache: { read: 0, write: 0 } },
    }),
    'utf8',
  );
  await writeFile(
    path.join(partDir, 'prt_oc_graph_tool.json'),
    JSON.stringify({
      id: 'prt_oc_graph_tool',
      sessionID: 'ses_oc_graph',
      messageID: 'msg_oc_graph_asst',
      type: 'tool',
      callID: 'call_oc_graph',
      tool: 'bash',
      state: {
        status: 'completed',
        input: { command: 'npm test' },
        output: 'failed',
        metadata: { exit: 1 },
      },
    }),
    'utf8',
  );
}
