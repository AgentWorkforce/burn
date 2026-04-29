import { strict as assert } from 'node:assert';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, afterEach, beforeEach, describe, it } from 'node:test';

import {
  __resetIndexCacheForTesting,
  ledgerPath,
  loadCursors,
  queryAll,
  queryRelationships,
  queryToolResultEvents,
  queryUserTurns,
  readContent,
} from '@relayburn/ledger';

import {
  ingestClaudeHookPayload,
  runIngest,
  runIngestTick,
  startWatchLoop,
} from './ingest.js';

function toolTranscript(
  sessionId: string,
  cwd: string,
  toolUseId: string,
  content: string,
  isError = false,
): string {
  return [
    JSON.stringify({
      parentUuid: null,
      isSidechain: false,
      type: 'user',
      message: { role: 'user', content: 'run the tool' },
      uuid: `${toolUseId}-user-1`,
      timestamp: '2026-04-22T00:00:00.000Z',
      cwd,
      sessionId,
    }),
    JSON.stringify({
      parentUuid: `${toolUseId}-user-1`,
      isSidechain: false,
      type: 'assistant',
      message: {
        model: 'claude-sonnet-4-6',
        id: `${toolUseId}-msg-1`,
        type: 'message',
        role: 'assistant',
        content: [
          { type: 'tool_use', id: toolUseId, name: 'Bash', input: { command: 'npm test' } },
        ],
        stop_reason: 'tool_use',
        usage: {
          input_tokens: 3,
          output_tokens: 5,
          cache_read_input_tokens: 0,
          cache_creation_input_tokens: 0,
          cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 },
        },
      },
      uuid: `${toolUseId}-asst-1`,
      timestamp: '2026-04-22T00:00:01.000Z',
      cwd,
      sessionId,
    }),
    JSON.stringify({
      parentUuid: `${toolUseId}-asst-1`,
      isSidechain: false,
      type: 'user',
      message: {
        role: 'user',
        content: [
          { type: 'tool_result', tool_use_id: toolUseId, content, is_error: isError },
        ],
      },
      uuid: `${toolUseId}-user-2`,
      timestamp: '2026-04-22T00:00:02.000Z',
      cwd,
      sessionId,
    }),
    '',
  ].join('\n');
}

function multiToolTranscript(
  sessionId: string,
  cwd: string,
  tools: Array<{ toolUseId: string; content: string; isError?: boolean }>,
): string {
  const lines: string[] = [
    JSON.stringify({
      parentUuid: null,
      isSidechain: false,
      type: 'user',
      message: { role: 'user', content: 'run the tools' },
      uuid: 'multi-user-0',
      timestamp: '2026-04-22T00:00:00.000Z',
      cwd,
      sessionId,
    }),
  ];
  let parentUuid = 'multi-user-0';
  tools.forEach((tool, idx) => {
    const assistantUuid = `multi-asst-${idx}`;
    lines.push(
      JSON.stringify({
        parentUuid,
        isSidechain: false,
        type: 'assistant',
        message: {
          model: 'claude-sonnet-4-6',
          id: `multi-msg-${idx}`,
          type: 'message',
          role: 'assistant',
          content: [
            {
              type: 'tool_use',
              id: tool.toolUseId,
              name: 'Bash',
              input: { command: `echo ${idx}` },
            },
          ],
          stop_reason: 'tool_use',
          usage: {
            input_tokens: 3,
            output_tokens: 5,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 },
          },
        },
        uuid: assistantUuid,
        timestamp: `2026-04-22T00:00:0${idx + 1}.000Z`,
        cwd,
        sessionId,
      }),
    );
    parentUuid = `multi-user-${idx + 1}`;
    lines.push(
      JSON.stringify({
        parentUuid: assistantUuid,
        isSidechain: false,
        type: 'user',
        message: {
          role: 'user',
          content: [
            {
              type: 'tool_result',
              tool_use_id: tool.toolUseId,
              content: tool.content,
              is_error: tool.isError === true,
            },
          ],
        },
        uuid: parentUuid,
        timestamp: `2026-04-22T00:00:0${idx + 2}.000Z`,
        cwd,
        sessionId,
      }),
    );
  });
  lines.push('');
  return lines.join('\n');
}

describe('burn ingest modes', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-ingest-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-ingest-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    delete process.env['RELAYBURN_CONTENT_STORE'];
    __resetIndexCacheForTesting();
  });

  afterEach(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('defaults to a one-shot scan of all session stores', async () => {
    await writeCodexSession(
      tmpHome,
      'ingest-once-rollout',
      codexCommittedSession('sess_ingest_codex', 'turn_ingest_codex', '/tmp/project'),
    );

    const output = await captureOutput(() =>
      runIngest({ flags: {}, tags: {}, positional: [], passthrough: [] }),
    );

    assert.equal(output.code, 0);
    assert.equal(output.stderr, '');
    assert.equal(output.stdout, '[burn] ingest: ingested 1 session (+1 turn)\n');
    const turns = await queryAll({ sessionId: 'sess_ingest_codex' });
    assert.equal(turns.length, 1);
    assert.equal(turns[0]!.source, 'codex');
  });

  it('warns when --interval is supplied outside --watch', async () => {
    const output = await captureOutput(() =>
      runIngest({ flags: { interval: '10' }, tags: {}, positional: [], passthrough: [] }),
    );

    assert.equal(output.code, 0);
    assert.match(output.stderr, /--interval is only used with --watch; ignoring/);
    assert.equal(output.stdout, '[burn] ingest: ingested 0 sessions (+0 turns)\n');
  });

  it('rejects mutually exclusive watch and hook modes', async () => {
    const output = await captureOutput(() =>
      runIngest({
        flags: { watch: true, hook: 'claude' },
        tags: {},
        positional: [],
        passthrough: [],
      }),
    );

    assert.equal(output.code, 2);
    assert.equal(output.stdout, '');
    assert.match(output.stderr, /--watch and --hook are mutually exclusive/);
  });

  it('single tick ingests newly committed codex turns through the shared cursor path', async () => {
    await writeCodexSession(
      tmpHome,
      'watch-rollout',
      codexCommittedSession('sess_watch_codex', 'turn_watch_codex', '/tmp/project'),
    );

    const report = await runIngestTick();

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

describe('burn ingest (hook-driven)', () => {
  let tmpRelay: string;
  let tmpTranscriptDir: string;
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalStore = process.env['RELAYBURN_CONTENT_STORE'];

  beforeEach(async () => {
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-ingest-relay-'));
    tmpTranscriptDir = await mkdtemp(path.join(tmpdir(), 'burn-ingest-tx-'));
    process.env['RELAYBURN_HOME'] = tmpRelay;
    delete process.env['RELAYBURN_CONTENT_STORE'];
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalStore !== undefined) process.env['RELAYBURN_CONTENT_STORE'] = originalStore;
    else delete process.env['RELAYBURN_CONTENT_STORE'];
    await rm(tmpRelay, { recursive: true, force: true });
    await rm(tmpTranscriptDir, { recursive: true, force: true });
  });

  it('parses a Claude transcript and appends turns + content sidecar', async () => {
    const sessionId = 'abcdef12-3456-7890-abcd-ef1234567890';
    const transcriptPath = path.join(tmpTranscriptDir, `${sessionId}.jsonl`);
    const toolResponseText = 'exact bytes from PostToolUse\n';
    const transcript = [
      JSON.stringify({
        parentUuid: null,
        isSidechain: false,
        type: 'user',
        message: { role: 'user', content: 'show the file' },
        uuid: 'u-user-1',
        timestamp: '2026-04-22T00:00:00.000Z',
        cwd: tmpTranscriptDir,
        sessionId,
      }),
      JSON.stringify({
        parentUuid: 'u-user-1',
        isSidechain: false,
        type: 'assistant',
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg-1',
          type: 'message',
          role: 'assistant',
          content: [
            { type: 'tool_use', id: 'toolu_1', name: 'Read', input: { file_path: '/x' } },
          ],
          stop_reason: 'tool_use',
          usage: {
            input_tokens: 3,
            output_tokens: 5,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 },
          },
        },
        uuid: 'u-asst-1',
        timestamp: '2026-04-22T00:00:01.000Z',
        cwd: tmpTranscriptDir,
        sessionId,
      }),
      JSON.stringify({
        parentUuid: 'u-asst-1',
        isSidechain: false,
        type: 'user',
        message: {
          role: 'user',
          content: [
            { type: 'tool_result', tool_use_id: 'toolu_1', content: toolResponseText },
          ],
        },
        uuid: 'u-user-2',
        timestamp: '2026-04-22T00:00:02.000Z',
        cwd: tmpTranscriptDir,
        sessionId,
      }),
      '',
    ].join('\n');
    await writeFile(transcriptPath, transcript, 'utf8');

    const payload = JSON.stringify({
      session_id: sessionId,
      transcript_path: transcriptPath,
      hook_event_name: 'PostToolUse',
    });

    const code = await ingestClaudeHookPayload(payload, { quiet: true });
    assert.equal(code, 0);

    const ledgerRaw = await readFile(ledgerPath(), 'utf8');
    const turnLines = ledgerRaw
      .split('\n')
      .filter((s) => s.length > 0)
      .map((s) => JSON.parse(s) as { kind: string; record?: { messageId?: string } });
    const turns = turnLines.filter((l) => l.kind === 'turn');
    assert.equal(turns.length, 1, 'one turn recorded');
    assert.equal(turns[0]!.record?.messageId, 'msg-1');

    const content = await readContent({ sessionId });
    const toolResult = content.find((c) => c.kind === 'tool_result');
    assert.ok(toolResult, 'tool_result content record exists');
    assert.equal(toolResult!.toolResult!.content, toolResponseText);
    assert.equal(toolResult!.toolResult!.toolUseId, 'toolu_1');

    const userTurns = await queryUserTurns({ sessionId });
    const toolResultUserTurn = userTurns.find((u) =>
      u.blocks.some((b) => b.kind === 'tool_result' && b.toolUseId === 'toolu_1'),
    );
    assert.ok(toolResultUserTurn, 'tool_result user-turn block is persisted');
    assert.equal(toolResultUserTurn!.precedingMessageId, 'msg-1');
    assert.equal(toolResultUserTurn!.blocks[0]!.approxTokens, 7);
  });

  it('emits PreToolUse and PostToolUse events before multi-tool reader replay dedupes', async () => {
    const sessionId = 'hook-prepost-session';
    const transcriptPath = path.join(tmpTranscriptDir, `${sessionId}.jsonl`);
    const tools = [
      { toolUseId: 'toolu_hook_pair_a', content: 'first passed\n' },
      { toolUseId: 'toolu_hook_pair_b', content: 'second passed\n' },
    ];
    await writeFile(transcriptPath, '', 'utf8');

    for (const tool of tools) {
      await ingestClaudeHookPayload(
        JSON.stringify({
          session_id: sessionId,
          transcript_path: transcriptPath,
          hook_event_name: 'PreToolUse',
          tool_name: 'Bash',
          tool_use_id: tool.toolUseId,
          tool_input: { command: 'npm test' },
        }),
        { quiet: true },
      );
    }
    for (const tool of tools) {
      await ingestClaudeHookPayload(
        JSON.stringify({
          session_id: sessionId,
          transcript_path: transcriptPath,
          hook_event_name: 'PostToolUse',
          tool_name: 'Bash',
          tool_use_id: tool.toolUseId,
          tool_response: tool.content,
        }),
        { quiet: true },
      );
    }
    await writeFile(
      transcriptPath,
      multiToolTranscript(sessionId, tmpTranscriptDir, tools),
      'utf8',
    );
    await ingestClaudeHookPayload(
      JSON.stringify({
        session_id: sessionId,
        transcript_path: transcriptPath,
        hook_event_name: 'SessionEnd',
      }),
      { quiet: true },
    );

    const events = await queryToolResultEvents({ sessionId });
    assert.equal(events.length, 4, 'reader replay should not add duplicate terminal events');
    assert.deepEqual(events.map((e) => e.eventIndex), [0, 1, 2, 3]);
    for (const tool of tools) {
      const running = events.find(
        (e) => e.toolUseId === tool.toolUseId && e.status === 'running',
      );
      const completed = events.find(
        (e) => e.toolUseId === tool.toolUseId && e.status === 'completed',
      );
      assert.ok(running, 'PreToolUse emits a running event');
      assert.ok(completed, 'PostToolUse emits the terminal event');
      assert.equal(running!.eventSource, 'tool_result');
      assert.equal(running!.callIndex, 0);
      assert.equal(completed!.eventSource, 'tool_result');
      assert.equal(completed!.callIndex, 1);
      assert.equal(completed!.contentLength, tool.content.length);
      assert.equal(typeof completed!.contentHash, 'string');
    }
  });

  it('marks PostToolUse hook responses with is_error as errored events', async () => {
    const sessionId = 'hook-post-error-session';
    const transcriptPath = path.join(tmpTranscriptDir, `${sessionId}.jsonl`);
    await writeFile(transcriptPath, '', 'utf8');

    await ingestClaudeHookPayload(
      JSON.stringify({
        session_id: sessionId,
        transcript_path: transcriptPath,
        hook_event_name: 'PostToolUse',
        tool_name: 'Bash',
        tool_use_id: 'toolu_hook_error',
        tool_response: { is_error: true, stderr: 'boom' },
      }),
      { quiet: true },
    );

    const events = await queryToolResultEvents({ sessionId });
    assert.equal(events.length, 1);
    assert.equal(events[0]!.status, 'errored');
    assert.equal(events[0]!.isError, true);
    assert.equal(events[0]!.callIndex, 0);
    assert.equal(events[0]!.eventIndex, 0);
    assert.equal(typeof events[0]!.contentHash, 'string');
  });

  it('emits SubagentStop notifications and subagent relationships', async () => {
    const sessionId = 'hook-subagent-session';
    const transcriptPath = path.join(tmpTranscriptDir, `${sessionId}.jsonl`);
    await writeFile(transcriptPath, '', 'utf8');

    await ingestClaudeHookPayload(
      JSON.stringify({
        session_id: sessionId,
        transcript_path: transcriptPath,
        hook_event_name: 'SubagentStop',
        tool_use_id: 'toolu_agent_spawn',
        agent_id: 'agent-123',
        agent_type: 'Explore',
        last_assistant_message: 'done',
      }),
      { quiet: true },
    );

    const events = await queryToolResultEvents({ sessionId });
    assert.equal(events.length, 1);
    assert.equal(events[0]!.eventSource, 'subagent_notification');
    assert.equal(events[0]!.status, 'completed');
    assert.equal(events[0]!.toolUseId, 'toolu_agent_spawn');
    assert.equal(events[0]!.agentId, 'agent-123');
    assert.equal(events[0]!.contentLength, 'done'.length);

    const relationships = await queryRelationships({ sessionId });
    assert.equal(relationships.length, 1);
    assert.equal(relationships[0]!.relationshipType, 'subagent');
    assert.equal(relationships[0]!.sessionId, sessionId);
    assert.equal(relationships[0]!.relatedSessionId, sessionId);
    assert.equal(relationships[0]!.agentId, 'agent-123');
    assert.equal(relationships[0]!.parentToolUseId, 'toolu_agent_spawn');
    assert.equal(relationships[0]!.subagentType, 'Explore');
  });

  it('is idempotent across repeat hook invocations on the same transcript', async () => {
    const sessionId = 'fedcba98-7654-3210-fedc-ba9876543210';
    const transcriptPath = path.join(tmpTranscriptDir, `${sessionId}.jsonl`);
    const transcript = [
      JSON.stringify({
        parentUuid: null,
        isSidechain: false,
        type: 'user',
        message: { role: 'user', content: 'hi' },
        uuid: 'u-user-1',
        timestamp: '2026-04-22T00:00:00.000Z',
        cwd: tmpTranscriptDir,
        sessionId,
      }),
      JSON.stringify({
        parentUuid: 'u-user-1',
        isSidechain: false,
        type: 'assistant',
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg-seed',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'text', text: 'hi there' }],
          stop_reason: 'end_turn',
          usage: {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 },
          },
        },
        uuid: 'u-asst-1',
        timestamp: '2026-04-22T00:00:01.000Z',
        cwd: tmpTranscriptDir,
        sessionId,
      }),
      '',
    ].join('\n');
    await writeFile(transcriptPath, transcript, 'utf8');

    const payload = JSON.stringify({
      session_id: sessionId,
      transcript_path: transcriptPath,
      hook_event_name: 'SessionEnd',
    });

    await ingestClaudeHookPayload(payload, { quiet: true });
    await ingestClaudeHookPayload(payload, { quiet: true });
    await ingestClaudeHookPayload(payload, { quiet: true });

    const ledgerRaw = await readFile(ledgerPath(), 'utf8');
    const turns = ledgerRaw
      .split('\n')
      .filter((s) => s.length > 0)
      .map((s) => JSON.parse(s) as { kind: string })
      .filter((l) => l.kind === 'turn');
    assert.equal(turns.length, 1, 'dedup keeps ledger at one turn across repeat fires');

    // First pass emits two content records (user prompt + assistant reply).
    // Passes 2 and 3 must add nothing because the cursor is at EOF.
    const content = await readContent({ sessionId });
    assert.equal(
      content.length,
      2,
      'cursor gate prevents content from being re-appended on repeat fires',
    );

    const cursors = await loadCursors();
    const cursor = cursors[transcriptPath];
    assert.ok(cursor, 'cursor saved for the transcript path');
    assert.equal(cursor!.kind, 'claude');
  });

  it('ignores empty payloads and malformed payloads without throwing', async () => {
    const origWrite = process.stderr.write.bind(process.stderr);
    process.stderr.write = ((_c: string | Uint8Array): boolean => true) as typeof process.stderr.write;
    try {
      assert.equal(await ingestClaudeHookPayload('', { quiet: true }), 0);
      assert.equal(await ingestClaudeHookPayload('{"foo":', { quiet: true }), 1);
      assert.equal(
        await ingestClaudeHookPayload('{"hook_event_name":"Stop"}', { quiet: true }),
        0,
      );
    } finally {
      process.stderr.write = origWrite;
    }
  });
});

async function captureOutput(fn: () => Promise<number>): Promise<{
  code: number;
  stdout: string;
  stderr: string;
}> {
  const origOut = process.stdout.write.bind(process.stdout);
  const origErr = process.stderr.write.bind(process.stderr);
  let stdout = '';
  let stderr = '';
  process.stdout.write = ((chunk: string | Uint8Array): boolean => {
    stdout += String(chunk);
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((chunk: string | Uint8Array): boolean => {
    stderr += String(chunk);
    return true;
  }) as typeof process.stderr.write;
  try {
    const code = await fn();
    return { code, stdout, stderr };
  } finally {
    process.stdout.write = origOut;
    process.stderr.write = origErr;
  }
}

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
