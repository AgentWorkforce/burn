import { strict as assert } from 'node:assert';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import {
  ledgerPath,
  loadCursors,
  queryUserTurns,
  readContent,
} from '@relayburn/ledger';

import { ingestClaudeHookPayload } from './ingest.js';

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
    assert.equal(toolResultUserTurn!.blocks[0]!.approxTokens, Math.ceil(toolResponseText.length / 4));
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
