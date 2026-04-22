import { strict as assert } from 'node:assert';
import { copyFile, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import { parseClaudeSession, parseClaudeSessionIncremental } from './claude.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures', 'claude');

describe('parseClaudeSession', () => {
  it('parses a simple one-turn session', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'simple-turn.jsonl'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.equal(t.v, 1);
    assert.equal(t.source, 'claude-code');
    assert.equal(t.messageId, 'msg_simple_1');
    assert.equal(t.model, 'claude-sonnet-4-6');
    assert.equal(t.project, '/tmp/project');
    assert.equal(t.stopReason, 'end_turn');
    assert.deepEqual(t.usage, {
      input: 10,
      output: 5,
      reasoning: 0,
      cacheRead: 500,
      cacheCreate5m: 80,
      cacheCreate1h: 20,
    });
    assert.equal(t.toolCalls.length, 0);
    assert.equal(t.filesTouched, undefined);
  });

  it('dedupes a multi-block assistant message and keeps usage once', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    assert.equal(turns.length, 1, 'four assistant lines with same messageId must collapse to one turn');
    const t = turns[0]!;
    assert.equal(t.messageId, 'msg_multi_1');
    assert.deepEqual(t.usage, {
      input: 3,
      output: 43,
      reasoning: 0,
      cacheRead: 11496,
      cacheCreate5m: 0,
      cacheCreate1h: 4773,
    });
    assert.equal(t.toolCalls.length, 2);
    assert.equal(t.toolCalls[0]!.name, 'Bash');
    assert.equal(t.toolCalls[0]!.target, 'ls -la /tmp/project');
    assert.equal(t.toolCalls[1]!.name, 'Agent');
    assert.equal(t.toolCalls[1]!.target, 'general-purpose');
    assert.equal(t.stopReason, 'tool_use');
    assert.equal(t.ts, '2026-04-20T00:00:01.000Z', 'ts is from the first assistant line for this msg');
  });

  it('extracts filesTouched only for Read/Edit/Write, not Grep/Bash', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'files-touched.jsonl'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.equal(t.toolCalls.length, 3);
    assert.deepEqual(t.filesTouched, ['/src/a.ts', '/src/b.ts']);
  });

  it('marks sidechain turns as subagent', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'sidechain-turn.jsonl'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.ok(t.subagent);
    assert.equal(t.subagent!.isSidechain, true);
  });

  it('produces stable argsHash for identical tool inputs', async () => {
    const a = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    const b = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    assert.equal(a.turns[0]!.toolCalls[0]!.argsHash, b.turns[0]!.toolCalls[0]!.argsHash);
    assert.notEqual(a.turns[0]!.toolCalls[0]!.argsHash, a.turns[0]!.toolCalls[1]!.argsHash);
  });
});

describe('parseClaudeSession content capture', () => {
  it('returns empty content array when contentMode is off (default)', async () => {
    const { content } = await parseClaudeSession(path.join(FIXTURES, 'simple-turn.jsonl'));
    assert.deepEqual(content, []);
  });

  it('returns empty content array when contentMode is hash-only', async () => {
    const { content } = await parseClaudeSession(path.join(FIXTURES, 'simple-turn.jsonl'), {
      contentMode: 'hash-only',
    });
    assert.deepEqual(content, []);
  });

  it('captures user text and assistant text when contentMode is full', async () => {
    const { content } = await parseClaudeSession(path.join(FIXTURES, 'simple-turn.jsonl'), {
      contentMode: 'full',
    });
    assert.equal(content.length, 2);
    const user = content.find((c) => c.role === 'user');
    assert.ok(user);
    assert.equal(user!.kind, 'text');
    assert.equal(user!.text, 'hello');
    assert.equal(user!.sessionId, '11111111-1111-1111-1111-111111111111');
    const asst = content.find((c) => c.role === 'assistant');
    assert.ok(asst);
    assert.equal(asst!.kind, 'text');
    assert.equal(asst!.text, 'Hello!');
    assert.equal(asst!.messageId, 'msg_simple_1');
    assert.equal(asst!.source, 'claude-code');
  });

  it('preserves chronological order across interleaved user/assistant turns', async () => {
    const { content } = await parseClaudeSession(
      path.join(FIXTURES, 'interleaved-turns.jsonl'),
      { contentMode: 'full' },
    );
    const sequence = content.map((c) => `${c.role}:${c.text ?? ''}`);
    assert.deepEqual(sequence, [
      'user:first question',
      'assistant:first answer',
      'user:second question',
      'assistant:second answer',
    ]);
  });

  it('captures thinking and tool_use blocks from a multi-block turn', async () => {
    const { content } = await parseClaudeSession(
      path.join(FIXTURES, 'multi-block-turn.jsonl'),
      { contentMode: 'full' },
    );
    const asstBlocks = content.filter((c) => c.role === 'assistant');
    const kinds = asstBlocks.map((c) => c.kind).sort();
    // Thinking block has empty text so it's omitted; we should see text + 2 tool_use.
    assert.deepEqual(kinds, ['text', 'tool_use', 'tool_use']);
    const toolUses = asstBlocks.filter((c) => c.kind === 'tool_use');
    assert.equal(toolUses[0]!.toolUse!.name, 'Bash');
    assert.deepEqual(toolUses[0]!.toolUse!.input, { command: 'ls -la /tmp/project' });
    assert.equal(toolUses[1]!.toolUse!.name, 'Agent');
  });
});

describe('parseClaudeSessionIncremental', () => {
  let tmp: string;

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-claude-inc-'));
  });

  afterEach(async () => {
    await rm(tmp, { recursive: true, force: true });
  });

  it('reads the whole file from startOffset=0 and returns endOffset at EOF', async () => {
    const src = path.join(FIXTURES, 'simple-turn.jsonl');
    const raw = await readFile(src, 'utf8');
    const { turns, endOffset } = await parseClaudeSessionIncremental(src);
    assert.equal(turns.length, 1);
    assert.equal(turns[0]!.messageId, 'msg_simple_1');
    assert.equal(endOffset, Buffer.byteLength(raw, 'utf8'));
  });

  it('returns zero turns when startOffset is already at EOF', async () => {
    const src = path.join(FIXTURES, 'simple-turn.jsonl');
    const raw = await readFile(src);
    const { turns, endOffset } = await parseClaudeSessionIncremental(src, {
      startOffset: raw.length,
    });
    assert.equal(turns.length, 0);
    assert.equal(endOffset, raw.length);
  });

  it('appending a complete turn yields only the new turn on next call', async () => {
    const src = path.join(FIXTURES, 'simple-turn.jsonl');
    const working = path.join(tmp, 'session.jsonl');
    await copyFile(src, working);
    const first = await parseClaudeSessionIncremental(working);
    assert.equal(first.turns.length, 1);

    // Append a second complete turn
    const appended = [
      JSON.stringify({
        parentUuid: 'u-asst-1',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_simple_2',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'text', text: 'and another' }],
          stop_reason: 'end_turn',
          usage: {
            input_tokens: 2,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 },
          },
        },
        type: 'assistant',
        uuid: 'u-asst-2',
        timestamp: '2026-04-20T00:00:05.000Z',
        cwd: '/tmp/project',
        sessionId: '11111111-1111-1111-1111-111111111111',
      }),
      '',
    ].join('\n');
    const prev = await readFile(working, 'utf8');
    await writeFile(working, prev + appended, 'utf8');

    const second = await parseClaudeSessionIncremental(working, { startOffset: first.endOffset });
    assert.equal(second.turns.length, 1);
    assert.equal(second.turns[0]!.messageId, 'msg_simple_2');
    const full = await readFile(working);
    assert.equal(second.endOffset, full.length);
  });

  it('defers an in-progress trailing message (endOffset before its first byte)', async () => {
    const src = path.join(FIXTURES, 'incomplete-then-complete.jsonl');
    const raw = await readFile(src, 'utf8');
    const inprogLine = '"id":"msg_inprog_1"';
    const inprogLineStart = raw.indexOf(
      raw
        .split('\n')
        .find((l) => l.includes(inprogLine))!,
    );
    const { turns, endOffset } = await parseClaudeSessionIncremental(src);
    assert.equal(turns.length, 1, 'only the complete message is emitted');
    assert.equal(turns[0]!.messageId, 'msg_done_1');
    assert.equal(endOffset, inprogLineStart, 'endOffset backs up to start of in-progress line');
  });

  it('does not emit content for in-progress messages, emits it once they complete', async () => {
    const src = path.join(FIXTURES, 'incomplete-then-complete.jsonl');
    const working = path.join(tmp, 'session.jsonl');
    await copyFile(src, working);
    const first = await parseClaudeSessionIncremental(working, { contentMode: 'full' });
    // Only the completed message's assistant content is emitted.
    const asst = first.content.filter((c) => c.role === 'assistant');
    assert.ok(asst.every((c) => c.messageId === 'msg_done_1'));

    // Append the completion line for msg_inprog_1
    const tailLine =
      JSON.stringify({
        parentUuid: 'u-asst-1',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_inprog_1',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'text', text: 'done now' }],
          stop_reason: 'end_turn',
          usage: {
            input_tokens: 7,
            output_tokens: 3,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 },
          },
        },
        type: 'assistant',
        uuid: 'u-asst-2',
        timestamp: '2026-04-20T00:00:02.000Z',
        cwd: '/tmp/project',
        sessionId: '33333333-3333-3333-3333-333333333333',
      }) + '\n';
    const prev = await readFile(working, 'utf8');
    await writeFile(working, prev + tailLine, 'utf8');

    const second = await parseClaudeSessionIncremental(working, {
      startOffset: first.endOffset,
      contentMode: 'full',
    });
    const laterAsst = second.content.filter((c) => c.role === 'assistant');
    // endOffset backs up to the start of the in-progress line on first pass,
    // so the second pass re-reads both the "working..." streamed block and
    // the completing "done now" block — both belong to msg_inprog_1.
    assert.ok(laterAsst.length >= 1);
    assert.ok(laterAsst.every((c) => c.messageId === 'msg_inprog_1'));
    assert.ok(laterAsst.some((c) => c.text === 'done now'));
  });

  it('defers assistant content for a complete message that appears after an in-progress one', async () => {
    // Construct a session where msg_done_1 (complete) is followed by an
    // in-progress msg_inprog_1 and then a trailing complete msg_after_1.
    // endOffset backs up to msg_inprog_1's start, so msg_after_1's content
    // must NOT be emitted yet — otherwise it would be duplicated on the next
    // incremental pass (there's no content-level dedup in appendContent).
    const working = path.join(tmp, 'session.jsonl');
    const lines = [
      JSON.stringify({
        parentUuid: null,
        isSidechain: false,
        type: 'user',
        message: { role: 'user', content: 'hi' },
        uuid: 'u-user-1',
        timestamp: '2026-04-20T00:00:00.000Z',
        cwd: '/tmp/project',
        sessionId: 'sess-dup',
      }),
      JSON.stringify({
        parentUuid: 'u-user-1',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_done_1',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'text', text: 'done' }],
          stop_reason: 'end_turn',
          usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0, cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 } },
        },
        type: 'assistant',
        uuid: 'u-asst-1',
        timestamp: '2026-04-20T00:00:01.000Z',
        cwd: '/tmp/project',
        sessionId: 'sess-dup',
      }),
      JSON.stringify({
        parentUuid: 'u-asst-1',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_inprog_1',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'text', text: 'working...' }],
          stop_reason: null,
          usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0, cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 } },
        },
        type: 'assistant',
        uuid: 'u-asst-2',
        timestamp: '2026-04-20T00:00:02.000Z',
        cwd: '/tmp/project',
        sessionId: 'sess-dup',
      }),
      JSON.stringify({
        parentUuid: 'u-asst-2',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_after_1',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'text', text: 'after' }],
          stop_reason: 'end_turn',
          usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0, cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 } },
        },
        type: 'assistant',
        uuid: 'u-asst-3',
        timestamp: '2026-04-20T00:00:03.000Z',
        cwd: '/tmp/project',
        sessionId: 'sess-dup',
      }),
    ];
    await writeFile(working, lines.join('\n') + '\n', 'utf8');

    const { content, endOffset } = await parseClaudeSessionIncremental(working, {
      contentMode: 'full',
    });
    const messageIds = content.filter((c) => c.role === 'assistant').map((c) => c.messageId);
    // Only msg_done_1 content should be committed this pass.
    assert.deepEqual(messageIds, ['msg_done_1']);
    // endOffset is before msg_inprog_1's first byte, so msg_after_1's bytes
    // are in the deferred region and will be re-read on the next call.
    const buf = await readFile(working);
    assert.ok(endOffset < buf.length);
  });

  it('skips incomplete turns and re-emits them after stop_reason arrives', async () => {
    const src = path.join(FIXTURES, 'incomplete-then-complete.jsonl');
    const working = path.join(tmp, 'session.jsonl');
    await copyFile(src, working);
    const first = await parseClaudeSessionIncremental(working);
    assert.equal(first.turns.length, 1);

    // Simulate the in-progress message completing: append a new line that
    // adds stop_reason for msg_inprog_1. We replace the whole tail by writing
    // the file again with the final line having stop_reason set.
    const tailLine =
      JSON.stringify({
        parentUuid: 'u-asst-1',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_inprog_1',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'text', text: 'working...' }],
          stop_reason: 'end_turn',
          usage: {
            input_tokens: 7,
            output_tokens: 3,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 },
          },
        },
        type: 'assistant',
        uuid: 'u-asst-2',
        timestamp: '2026-04-20T00:00:02.000Z',
        cwd: '/tmp/project',
        sessionId: '33333333-3333-3333-3333-333333333333',
      }) + '\n';
    // Append this completing line after the existing in-progress one
    const prev = await readFile(working, 'utf8');
    await writeFile(working, prev + tailLine, 'utf8');

    const second = await parseClaudeSessionIncremental(working, { startOffset: first.endOffset });
    assert.equal(second.turns.length, 1);
    assert.equal(second.turns[0]!.messageId, 'msg_inprog_1');
    assert.equal(second.turns[0]!.stopReason, 'end_turn');
  });
});
