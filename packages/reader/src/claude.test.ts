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
    const turns = await parseClaudeSession(path.join(FIXTURES, 'simple-turn.jsonl'));
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
    const turns = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
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
    const turns = await parseClaudeSession(path.join(FIXTURES, 'files-touched.jsonl'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.equal(t.toolCalls.length, 3);
    assert.deepEqual(t.filesTouched, ['/src/a.ts', '/src/b.ts']);
  });

  it('marks sidechain turns as subagent', async () => {
    const turns = await parseClaudeSession(path.join(FIXTURES, 'sidechain-turn.jsonl'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.ok(t.subagent);
    assert.equal(t.subagent!.isSidechain, true);
  });

  it('produces stable argsHash for identical tool inputs', async () => {
    const a = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    const b = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    assert.equal(a[0]!.toolCalls[0]!.argsHash, b[0]!.toolCalls[0]!.argsHash);
    assert.notEqual(a[0]!.toolCalls[0]!.argsHash, a[0]!.toolCalls[1]!.argsHash);
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
