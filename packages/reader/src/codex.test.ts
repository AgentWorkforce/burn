import { strict as assert } from 'node:assert';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { parseCodexSession, parseCodexSessionIncremental } from './codex.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures', 'codex');

describe('parseCodexSession', () => {
  it('parses a simple one-turn session', async () => {
    const { turns } = await parseCodexSession(path.join(FIXTURES, 'simple-turn.jsonl'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.equal(t.v, 1);
    assert.equal(t.source, 'codex');
    assert.equal(t.sessionId, 'sess_simple_1');
    assert.equal(t.messageId, 'turn_simple_1');
    assert.equal(t.turnIndex, 0);
    assert.equal(t.model, 'gpt-5.4');
    assert.equal(t.project, '/tmp/project');
    assert.equal(t.ts, '2026-04-20T00:00:00.200Z');
    assert.deepEqual(t.usage, {
      input: 600,
      output: 120,
      reasoning: 30,
      cacheRead: 400,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    });
    assert.equal(t.toolCalls.length, 0);
    assert.equal(t.filesTouched, undefined);
  });

  it('extracts function and custom tool calls and maps filesTouched from patch_apply_end', async () => {
    const { turns } = await parseCodexSession(path.join(FIXTURES, 'with-tool-call.jsonl'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.equal(t.model, 'gpt-5.3-codex');
    assert.deepEqual(t.usage, {
      input: 3000,
      output: 800,
      reasoning: 200,
      cacheRead: 2000,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    });
    assert.equal(t.toolCalls.length, 3);

    const [exec, patch1, patch2] = t.toolCalls;
    assert.equal(exec!.name, 'exec_command');
    assert.equal(exec!.target, 'git status');

    assert.equal(patch1!.name, 'apply_patch');
    assert.equal(patch1!.target, '/tmp/project/README.md');

    assert.equal(patch2!.name, 'apply_patch');
    assert.equal(patch2!.target, '/tmp/project/NEW.md');

    assert.deepEqual(t.filesTouched?.sort(), [
      '/tmp/project/NEW.md',
      '/tmp/project/README.md',
    ]);
  });

  it('computes per-turn usage as delta of cumulative totals across multiple turns', async () => {
    const { turns } = await parseCodexSession(path.join(FIXTURES, 'multi-turn.jsonl'));
    assert.equal(turns.length, 2);

    const [t1, t2] = turns;
    assert.equal(t1!.messageId, 'turn_multi_1');
    assert.equal(t1!.turnIndex, 0);
    assert.equal(t1!.model, 'gpt-5.4');
    assert.deepEqual(t1!.usage, {
      input: 2000,
      output: 200,
      reasoning: 50,
      cacheRead: 1000,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    });

    assert.equal(t2!.messageId, 'turn_multi_2');
    assert.equal(t2!.turnIndex, 1);
    assert.equal(t2!.model, 'gpt-5.3-codex');
    assert.deepEqual(t2!.usage, {
      input: 2500,
      output: 500,
      reasoning: 50,
      cacheRead: 2500,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    });
    assert.equal(t2!.toolCalls.length, 1);
    assert.equal(t2!.toolCalls[0]!.name, 'exec_command');
    assert.equal(t2!.toolCalls[0]!.target, 'ls');
  });

  it('produces stable argsHash for identical tool inputs', async () => {
    const a = await parseCodexSession(path.join(FIXTURES, 'with-tool-call.jsonl'));
    const b = await parseCodexSession(path.join(FIXTURES, 'with-tool-call.jsonl'));
    assert.equal(a.turns[0]!.toolCalls[0]!.argsHash, b.turns[0]!.toolCalls[0]!.argsHash);
    assert.notEqual(a.turns[0]!.toolCalls[0]!.argsHash, a.turns[0]!.toolCalls[1]!.argsHash);
  });

  it('respects sessionPath option', async () => {
    const file = path.join(FIXTURES, 'simple-turn.jsonl');
    const { turns } = await parseCodexSession(file, { sessionPath: file });
    assert.equal(turns[0]!.sessionPath, file);
  });

  it('classifies activity and fills retries/hasEdits for codex turns', async () => {
    const { turns } = await parseCodexSession(path.join(FIXTURES, 'with-tool-call.jsonl'));
    const t = turns[0]!;
    // apply_patch normalized → Edit, so hasEdits should be true. Both patches
    // target .md files (README.md, NEW.md), so this turn lands in 'docs'.
    assert.equal(t.hasEdits, true);
    assert.equal(t.activity, 'docs');
    assert.equal(typeof t.retries, 'number');
  });

  it('marks exec_command_end with non-zero exit as a failed tool → debugging', async () => {
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-fail-'));
    try {
      const jsonl =
        [
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.000Z',
            type: 'session_meta',
            payload: { id: 'sess_fail', cwd: '/tmp/proj', timestamp: '2026-04-22T00:00:00.000Z' },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.100Z',
            type: 'turn_context',
            payload: { turn_id: 'turn_fail_1', cwd: '/tmp/proj', model: 'gpt-5.4' },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.200Z',
            type: 'response_item',
            payload: {
              type: 'message',
              role: 'user',
              content: [{ type: 'input_text', text: 'run the tests please' }],
            },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.300Z',
            type: 'event_msg',
            payload: { type: 'task_started', turn_id: 'turn_fail_1' },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:01.000Z',
            type: 'response_item',
            payload: {
              type: 'function_call',
              name: 'exec_command',
              arguments: '{"cmd":"pytest -q"}',
              call_id: 'call_fail_1',
            },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:01.500Z',
            type: 'event_msg',
            payload: { type: 'exec_command_end', call_id: 'call_fail_1', turn_id: 'turn_fail_1', exit_code: 1 },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:02.000Z',
            type: 'event_msg',
            payload: { type: 'token_count', info: { total_token_usage: { input_tokens: 100, cached_input_tokens: 0, output_tokens: 50 } } },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:02.100Z',
            type: 'event_msg',
            payload: { type: 'task_complete', turn_id: 'turn_fail_1' },
          }),
          '',
        ].join('\n');
      const file = path.join(tmp, 'fail.jsonl');
      await writeFile(file, jsonl, 'utf8');
      const { turns } = await parseCodexSession(file);
      assert.equal(turns.length, 1);
      // pytest would be 'testing', but the failed exec_command promotes it to debugging.
      assert.equal(turns[0]!.activity, 'debugging');
      assert.equal(turns[0]!.hasEdits, false);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });

  it('uses the user prompt for keyword refinement, skipping codex boilerplate', async () => {
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-kw-'));
    try {
      const jsonl =
        [
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.000Z',
            type: 'session_meta',
            payload: { id: 'sess_kw', cwd: '/tmp/proj', timestamp: '2026-04-22T00:00:00.000Z' },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.050Z',
            type: 'turn_context',
            payload: { turn_id: 'turn_kw_1', cwd: '/tmp/proj', model: 'gpt-5.4' },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.100Z',
            type: 'response_item',
            payload: {
              type: 'message',
              role: 'user',
              content: [
                { type: 'input_text', text: '<environment_context><cwd>/tmp/proj</cwd></environment_context>' },
                { type: 'input_text', text: 'refactor the auth module to extract the token helper' },
              ],
            },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:00.200Z',
            type: 'event_msg',
            payload: { type: 'task_started', turn_id: 'turn_kw_1' },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:01.000Z',
            type: 'response_item',
            payload: {
              type: 'custom_tool_call',
              name: 'apply_patch',
              input: '*** Begin Patch\n*** Update File: /tmp/proj/auth.ts\n@@\n+ok\n*** End Patch\n',
              call_id: 'call_kw_1',
            },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:01.200Z',
            type: 'event_msg',
            payload: {
              type: 'patch_apply_end',
              call_id: 'call_kw_1',
              turn_id: 'turn_kw_1',
              success: true,
              changes: { '/tmp/proj/auth.ts': { type: 'update' } },
            },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:02.000Z',
            type: 'event_msg',
            payload: { type: 'token_count', info: { total_token_usage: { input_tokens: 100, cached_input_tokens: 0, output_tokens: 50 } } },
          }),
          JSON.stringify({
            timestamp: '2026-04-22T00:00:02.100Z',
            type: 'event_msg',
            payload: { type: 'task_complete', turn_id: 'turn_kw_1' },
          }),
          '',
        ].join('\n');
      const file = path.join(tmp, 'kw.jsonl');
      await writeFile(file, jsonl, 'utf8');
      const { turns } = await parseCodexSession(file);
      assert.equal(turns.length, 1);
      assert.equal(turns[0]!.activity, 'refactoring');
      assert.equal(turns[0]!.hasEdits, true);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });
});

describe('parseCodexSessionIncremental', () => {
  it('full parse from startOffset=0 matches parseCodexSession', async () => {
    const file = path.join(FIXTURES, 'multi-turn.jsonl');
    const expected = await parseCodexSession(file);
    const { turns, endOffset } = await parseCodexSessionIncremental(file);
    assert.equal(turns.length, expected.turns.length);
    const raw = await readFile(file);
    assert.equal(endOffset, raw.length);
  });

  it('splits at task_complete boundary and resumes with cumulative snapshot', async () => {
    const file = path.join(FIXTURES, 'multi-turn.jsonl');
    const raw = await readFile(file, 'utf8');
    const lines = raw.split('\n');
    // Offset right after the first task_complete line (line index 5, 0-based)
    const cutoff = Buffer.byteLength(lines.slice(0, 6).join('\n') + '\n', 'utf8');

    const first = await parseCodexSessionIncremental(file, {
      startOffset: 0,
    });
    // Simulate the scenario where only the first turn had completed by now:
    // split the stream at the first task_complete by passing a truncated buffer.
    // For this we rewrite to a temp-truncated view via a custom test file.
    // Simpler: verify that when we parse the full file, the resume returned
    // reflects the latest task_complete boundary (end of file here).
    assert.equal(first.endOffset, Buffer.byteLength(raw, 'utf8'));
    assert.equal(first.turns.length, 2);

    // Now parse from a mid-file startOffset simulating resumption:
    // assume the caller previously committed at `cutoff` with matching resume state.
    // Build the resume state by first doing a partial parse up to cutoff.
    // Easiest: write a temp file containing the first half, run parse, then
    // concat second half and run parse with resume.
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-inc-'));
    try {
      const partialPath = path.join(tmp, 'partial.jsonl');
      await writeFile(partialPath, raw.slice(0, cutoff), 'utf8');
      const partial = await parseCodexSessionIncremental(partialPath);
      assert.equal(partial.turns.length, 1);
      assert.equal(partial.turns[0]!.messageId, 'turn_multi_1');
      assert.equal(partial.endOffset, cutoff);

      // Now write the full file and resume
      const fullPath = path.join(tmp, 'full.jsonl');
      await writeFile(fullPath, raw, 'utf8');
      const resumed = await parseCodexSessionIncremental(fullPath, {
        startOffset: partial.endOffset,
        resume: partial.resume,
      });
      assert.equal(resumed.turns.length, 1);
      assert.equal(resumed.turns[0]!.messageId, 'turn_multi_2');
      // The second turn's usage must match the delta computed in the full parse
      const full = await parseCodexSession(fullPath);
      assert.deepEqual(resumed.turns[0]!.usage, full.turns[1]!.usage);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });

  it('does not advance endOffset if no task_complete seen in the tail', async () => {
    const file = path.join(FIXTURES, 'simple-turn.jsonl');
    const raw = await readFile(file, 'utf8');
    const lines = raw.split('\n');
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-noctc-'));
    try {
      // Keep only lines up to and including task_started (index 2), no task_complete
      const truncated = lines.slice(0, 3).join('\n') + '\n';
      const truncPath = path.join(tmp, 'trunc.jsonl');
      await writeFile(truncPath, truncated, 'utf8');
      const r = await parseCodexSessionIncremental(truncPath);
      assert.equal(r.turns.length, 0, 'no task_complete = no committed turns');
      assert.equal(r.endOffset, 0, 'endOffset stays at startOffset');
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });
});

describe('parseCodexSession content capture', () => {
  async function withFixture<T>(body: (file: string) => Promise<T>): Promise<T> {
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-content-'));
    try {
      const file = path.join(tmp, 'session.jsonl');
      const lines = [
        {
          timestamp: '2026-04-20T01:00:00.000Z',
          type: 'session_meta',
          payload: { id: 'sess_content_1', cwd: '/tmp/project', timestamp: '2026-04-20T01:00:00.000Z' },
        },
        {
          timestamp: '2026-04-20T01:00:00.050Z',
          type: 'turn_context',
          payload: { turn_id: 'turn_content_1', cwd: '/tmp/project', model: 'gpt-5.3-codex' },
        },
        {
          timestamp: '2026-04-20T01:00:00.100Z',
          type: 'response_item',
          payload: {
            type: 'message',
            role: 'user',
            content: [{ type: 'input_text', text: 'list files' }],
          },
        },
        {
          timestamp: '2026-04-20T01:00:00.200Z',
          type: 'event_msg',
          payload: { type: 'task_started', turn_id: 'turn_content_1' },
        },
        {
          timestamp: '2026-04-20T01:00:00.300Z',
          type: 'response_item',
          payload: {
            type: 'reasoning',
            summary: [{ type: 'summary_text', text: 'planning the ls' }],
            content: null,
          },
        },
        {
          timestamp: '2026-04-20T01:00:00.400Z',
          type: 'response_item',
          payload: {
            type: 'function_call',
            name: 'shell',
            arguments: '{"cmd":"ls"}',
            call_id: 'call_fc_1',
          },
        },
        {
          timestamp: '2026-04-20T01:00:00.500Z',
          type: 'response_item',
          payload: {
            type: 'function_call_output',
            call_id: 'call_fc_1',
            output: 'README.md\npackage.json\n',
          },
        },
        {
          timestamp: '2026-04-20T01:00:00.600Z',
          type: 'response_item',
          payload: {
            type: 'custom_tool_call',
            name: 'apply_patch',
            input: '*** Begin Patch\n*** Add File: /tmp/project/X\n',
            call_id: 'call_ct_1',
          },
        },
        {
          timestamp: '2026-04-20T01:00:00.700Z',
          type: 'response_item',
          payload: {
            type: 'custom_tool_call_output',
            call_id: 'call_ct_1',
            output: '{"success":true}',
          },
        },
        {
          timestamp: '2026-04-20T01:00:00.800Z',
          type: 'response_item',
          payload: {
            type: 'message',
            role: 'assistant',
            content: [{ type: 'output_text', text: 'done.' }],
          },
        },
        {
          timestamp: '2026-04-20T01:00:00.900Z',
          type: 'event_msg',
          payload: {
            type: 'token_count',
            info: { total_token_usage: { input_tokens: 100, cached_input_tokens: 0, output_tokens: 20, reasoning_output_tokens: 10 } },
          },
        },
        {
          timestamp: '2026-04-20T01:00:01.000Z',
          type: 'event_msg',
          payload: { type: 'task_complete', turn_id: 'turn_content_1' },
        },
      ];
      await writeFile(file, lines.map((l) => JSON.stringify(l)).join('\n') + '\n', 'utf8');
      return await body(file);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  }

  it('returns empty content when contentMode is off (default)', async () => {
    await withFixture(async (file) => {
      const { content } = await parseCodexSession(file);
      assert.deepEqual(content, []);
    });
  });

  it('returns empty content when contentMode is hash-only', async () => {
    await withFixture(async (file) => {
      const { content } = await parseCodexSession(file, { contentMode: 'hash-only' });
      assert.deepEqual(content, []);
    });
  });

  it('emits tool_result for function_call_output with matching toolUseId', async () => {
    await withFixture(async (file) => {
      const { turns, content } = await parseCodexSession(file, { contentMode: 'full' });
      assert.equal(turns.length, 1);
      const tr = content.find((c) => c.kind === 'tool_result' && c.toolResult?.toolUseId === 'call_fc_1');
      assert.ok(tr, 'tool_result for function_call_output is emitted');
      assert.equal(tr!.source, 'codex');
      assert.equal(tr!.toolResult!.content, 'README.md\npackage.json\n');
      const toolIds = new Set(turns[0]!.toolCalls.map((tc) => tc.id));
      assert.ok(toolIds.has('call_fc_1'), 'attributor can join tool_result to a toolCall');
    });
  });

  it('emits tool_result for custom_tool_call_output', async () => {
    await withFixture(async (file) => {
      const { content } = await parseCodexSession(file, { contentMode: 'full' });
      const tr = content.find((c) => c.kind === 'tool_result' && c.toolResult?.toolUseId === 'call_ct_1');
      assert.ok(tr);
      assert.equal(tr!.toolResult!.content, '{"success":true}');
    });
  });

  it('captures user/assistant text, reasoning, and tool_use blocks', async () => {
    await withFixture(async (file) => {
      const { content } = await parseCodexSession(file, { contentMode: 'full' });
      const user = content.find((c) => c.role === 'user' && c.kind === 'text');
      assert.equal(user!.text, 'list files');
      assert.equal(user!.messageId, 'turn_content_1', 'pre-turn user content re-anchors to the turn it opens');
      const asst = content.find((c) => c.role === 'assistant' && c.kind === 'text');
      assert.equal(asst!.text, 'done.');
      const thinking = content.find((c) => c.kind === 'thinking');
      assert.equal(thinking!.text, 'planning the ls');
      const toolUses = content.filter((c) => c.kind === 'tool_use');
      assert.equal(toolUses.length, 2);
      assert.deepEqual(
        toolUses.map((t) => t.toolUse!.name).sort(),
        ['apply_patch', 'shell'],
      );
    });
  });

  it('drops content when the enclosing turn never commits', async () => {
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-uncommitted-'));
    try {
      const file = path.join(tmp, 'session.jsonl');
      const lines = [
        { timestamp: '2026-04-20T01:00:00.000Z', type: 'session_meta', payload: { id: 'sess_u', cwd: '/tmp', timestamp: '2026-04-20T01:00:00.000Z' } },
        { timestamp: '2026-04-20T01:00:00.050Z', type: 'turn_context', payload: { turn_id: 'turn_u', cwd: '/tmp', model: 'gpt-5.4' } },
        { timestamp: '2026-04-20T01:00:00.200Z', type: 'event_msg', payload: { type: 'task_started', turn_id: 'turn_u' } },
        {
          timestamp: '2026-04-20T01:00:00.400Z',
          type: 'response_item',
          payload: { type: 'function_call', name: 'shell', arguments: '{"cmd":"ls"}', call_id: 'call_u_1' },
        },
        {
          timestamp: '2026-04-20T01:00:00.500Z',
          type: 'response_item',
          payload: { type: 'function_call_output', call_id: 'call_u_1', output: 'should-be-dropped' },
        },
        // no task_complete — turn is still open
      ];
      await writeFile(file, lines.map((l) => JSON.stringify(l)).join('\n') + '\n', 'utf8');
      const { turns, content } = await parseCodexSession(file, { contentMode: 'full' });
      assert.equal(turns.length, 0);
      assert.equal(content.length, 0, 'uncommitted content is not emitted');
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });
});

describe('parseCodexSession user-turn block sizes (issue #81)', () => {
  it('emits one UserTurnRecord per group of user-side blocks between assistant turns', async () => {
    const { turns, userTurns } = await parseCodexSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    assert.equal(turns.length, 3);
    // Three committed user-side groups: pre-turn-1 text, between 1 & 2, between 2 & 3.
    // The trailing tool output after turn 3 has no following turn, so it isn't emitted.
    assert.equal(userTurns.length, 3);

    for (const u of userTurns) {
      assert.equal(u.v, 1);
      assert.equal(u.source, 'codex');
      assert.equal(u.sessionId, 'sess_codex_utb');
      assert.equal(typeof u.userUuid, 'string');
      assert.ok(u.userUuid.length > 0, 'userUuid is non-empty');
      assert.ok(u.ts.length > 0, 'ts populated');
      assert.ok(u.blocks.length >= 1, 'at least one block');
    }

    const [pre, between12, between23] = userTurns;
    // Pre-turn-1: free-text prompt with no preceding turn, following = turn_utb_1.
    assert.equal(pre!.precedingMessageId, undefined);
    assert.equal(pre!.followingMessageId, 'turn_utb_1');
    assert.equal(pre!.blocks.length, 1);
    assert.equal(pre!.blocks[0]!.kind, 'text');
    assert.equal(pre!.blocks[0]!.byteLen, Buffer.byteLength('fix the build', 'utf8'));
    assert.equal(pre!.blocks[0]!.approxTokens, Math.ceil(13 / 4));

    // Between turn 1 and 2: tool output from turn 1 + inter-turn user text.
    assert.equal(between12!.precedingMessageId, 'turn_utb_1');
    assert.equal(between12!.followingMessageId, 'turn_utb_2');
    assert.equal(between12!.blocks.length, 2);
    const tr1 = between12!.blocks.find((b) => b.kind === 'tool_result');
    assert.ok(tr1);
    assert.equal(tr1!.toolUseId, 'call_b1');
    assert.equal(tr1!.byteLen, Buffer.byteLength('a\n', 'utf8'));
    assert.equal(tr1!.approxTokens, Math.ceil(2 / 4));
    assert.equal(tr1!.isError, undefined, 'successful exec → no isError');
    const txt = between12!.blocks.find((b) => b.kind === 'text');
    assert.ok(txt);
    assert.equal(txt!.byteLen, Buffer.byteLength('now run tests', 'utf8'));

    // Between turn 2 and 3: errored function_call_output + custom_tool_call_output.
    assert.equal(between23!.precedingMessageId, 'turn_utb_2');
    assert.equal(between23!.followingMessageId, 'turn_utb_3');
    assert.equal(between23!.blocks.length, 2);
    const failBlock = between23!.blocks.find((b) => b.toolUseId === 'call_b2');
    assert.ok(failBlock);
    assert.equal(failBlock!.kind, 'tool_result');
    assert.equal(failBlock!.byteLen, Buffer.byteLength('FAIL: 1 test broke', 'utf8'));
    assert.equal(failBlock!.isError, true, 'non-zero exit → isError');
    const patchBlock = between23!.blocks.find((b) => b.toolUseId === 'call_p1');
    assert.ok(patchBlock);
    assert.equal(patchBlock!.kind, 'tool_result');
    assert.equal(patchBlock!.byteLen, Buffer.byteLength('patched', 'utf8'));
    assert.equal(patchBlock!.isError, undefined);
  });

  it('reconciles per-turn input delta with sum of user-turn block tokens for the preceding gap', async () => {
    // Codex reports `cumulative` token counts; per-turn usage is the delta. The
    // non-cached input growth between turns N and N+1 should approximately
    // equal `output(N) + sum(approxTokens of user turn between N and N+1)`.
    // Issue #81: document the chosen reference (cumulative deltas via
    // total_token_usage, not last_token_usage).
    const { turns, userTurns } = await parseCodexSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    const byFollowing = new Map(userTurns.map((u) => [u.followingMessageId ?? '', u]));
    for (let i = 1; i < turns.length; i++) {
      const prev = turns[i - 1]!;
      const cur = turns[i]!;
      const u = byFollowing.get(cur.messageId);
      const userTurnTokens = u
        ? u.blocks.reduce((s, b) => s + b.approxTokens, 0)
        : 0;
      const lhs = cur.usage.input - prev.usage.output;
      assert.ok(lhs > 0, `input(${i}) - output(${i - 1}) should be positive`);
      // Order-of-magnitude check: the delta and the user turn tokens should
      // be within a factor of ~3 of each other on the fixture (typical
      // sessions sit closer; the loose bound absorbs cache/granularity slop).
      const ratio = lhs / Math.max(userTurnTokens, 1);
      assert.ok(
        ratio >= 1 / 3 && ratio <= 3,
        `input delta ${lhs} and user turn tokens ${userTurnTokens} differ by more than 3x`,
      );
    }
  });

  it('emits empty userTurns for sessions with no user-side blocks', async () => {
    const { userTurns } = await parseCodexSession(path.join(FIXTURES, 'simple-turn.jsonl'));
    assert.deepEqual(userTurns, []);
  });
});

describe('parseCodexSession execution graph (#42 / #87)', () => {
  it('emits exactly one root SessionRelationshipRecord per session', async () => {
    const { relationships } = await parseCodexSession(
      path.join(FIXTURES, 'simple-turn.jsonl'),
    );
    const roots = relationships.filter((r) => r.relationshipType === 'root');
    assert.equal(roots.length, 1, 'exactly one root row');
    assert.equal(roots[0]!.source, 'codex');
    assert.equal(roots[0]!.sessionId, 'sess_simple_1');
    assert.equal(roots[0]!.relatedSessionId, undefined);
    assert.equal(roots[0]!.ts, '2026-04-20T00:00:00.000Z');
  });

  it('emits a ToolResultEventRecord per function_call_output / custom_tool_call_output', async () => {
    const { toolResultEvents } = await parseCodexSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    // user-turn-blocks has 1 fc output in turn 1, 1 fc output + 1 ct output in
    // turn 2, and 1 fc output in turn 3 — but turn 3's output lives between
    // turn 3 and the end-of-file (no following task_complete? — there is one).
    const byCall = new Map<string, number>();
    for (const ev of toolResultEvents) {
      byCall.set(ev.toolUseId, (byCall.get(ev.toolUseId) ?? 0) + 1);
    }
    assert.equal(byCall.get('call_b1'), 1);
    assert.equal(byCall.get('call_b2'), 1);
    assert.equal(byCall.get('call_p1'), 1);
    assert.equal(byCall.get('call_b3'), 1);
    for (const ev of toolResultEvents) {
      assert.equal(ev.v, 1);
      assert.equal(ev.source, 'codex');
      assert.equal(ev.eventSource, 'function_call_output');
      assert.equal(typeof ev.eventIndex, 'number');
      assert.equal(typeof ev.contentLength, 'number');
      assert.equal(typeof ev.contentHash, 'string');
    }
  });

  it('eventIndex values are unique and session-monotonic', async () => {
    const { toolResultEvents } = await parseCodexSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    const indices = toolResultEvents.map((e) => e.eventIndex);
    const sorted = [...indices].sort((a, b) => a - b);
    assert.deepEqual(indices, sorted, 'eventIndex emitted in ascending order');
    assert.equal(new Set(indices).size, indices.length, 'no duplicates');
  });

  it('marks function_call_output as errored when exec_command_end has non-zero exit', async () => {
    const { toolResultEvents } = await parseCodexSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    // call_b2 had `exec_command_end` exit_code=1 in the fixture.
    const failed = toolResultEvents.find((e) => e.toolUseId === 'call_b2');
    assert.ok(failed);
    assert.equal(failed!.status, 'errored');
    assert.equal(failed!.isError, true);

    const ok = toolResultEvents.find((e) => e.toolUseId === 'call_b1');
    assert.ok(ok);
    assert.equal(ok!.status, 'completed');
    assert.equal(ok!.isError, undefined);
  });

  it('marks function_call_output as completed when patch_apply_end succeeds', async () => {
    const { toolResultEvents } = await parseCodexSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    const patched = toolResultEvents.find((e) => e.toolUseId === 'call_p1');
    assert.ok(patched);
    assert.equal(patched!.status, 'completed');
    assert.equal(patched!.isError, undefined);
  });

  it('emits a subagent SessionRelationshipRecord with parentToolUseId set to the spawning call', async () => {
    const { relationships } = await parseCodexSession(
      path.join(FIXTURES, 'with-spawn-agent.jsonl'),
    );
    const subagents = relationships.filter((r) => r.relationshipType === 'subagent');
    assert.equal(subagents.length, 1, 'one subagent row per spawn');
    const sub = subagents[0]!;
    assert.equal(sub.source, 'codex');
    assert.equal(sub.sessionId, 'agent_inv_42', 'row is keyed on the child session');
    assert.equal(sub.relatedSessionId, 'sess_spawn_1', 'parent is the codex session');
    assert.equal(sub.parentToolUseId, 'call_spawn_1');
    assert.equal(sub.agentId, 'agent_inv_42');
    assert.equal(sub.subagentType, 'investigator');
    assert.equal(sub.description, 'find why test_x fails');
    assert.ok(typeof sub.ts === 'string' && sub.ts.length > 0);
  });

  it('annotates the spawn function_call_output event with the spawned agentId', async () => {
    const { toolResultEvents } = await parseCodexSession(
      path.join(FIXTURES, 'with-spawn-agent.jsonl'),
    );
    const spawnOutput = toolResultEvents.find(
      (e) => e.toolUseId === 'call_spawn_1' && e.eventSource === 'function_call_output',
    );
    assert.ok(spawnOutput);
    assert.equal(spawnOutput!.agentId, 'agent_inv_42');
    assert.equal(spawnOutput!.subagentSessionId, 'agent_inv_42');
  });

  it('emits a subagent_notification ToolResultEventRecord joined back by toolUseId', async () => {
    const { toolResultEvents } = await parseCodexSession(
      path.join(FIXTURES, 'with-spawn-agent.jsonl'),
    );
    const notif = toolResultEvents.find(
      (e) => e.eventSource === 'subagent_notification' && e.toolUseId === 'call_spawn_1',
    );
    assert.ok(notif, 'subagent terminal notification surfaces as ToolResultEventRecord');
    assert.equal(notif!.status, 'completed');
    assert.equal(notif!.agentId, 'agent_inv_42');
    assert.equal(notif!.subagentSessionId, 'agent_inv_42');
  });

  it('emits exactly one root row even when re-parsing the file', async () => {
    const file = path.join(FIXTURES, 'with-spawn-agent.jsonl');
    const a = await parseCodexSession(file);
    const b = await parseCodexSession(file);
    const aRoots = a.relationships.filter((r) => r.relationshipType === 'root');
    const bRoots = b.relationships.filter((r) => r.relationshipType === 'root');
    assert.equal(aRoots.length, 1);
    assert.equal(bRoots.length, 1);
  });

  it('does not emit relationships or events for uncommitted (no task_complete) turns', async () => {
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-uncommitted-eg-'));
    try {
      const lines = [
        { timestamp: '2026-04-23T00:00:00.000Z', type: 'session_meta', payload: { id: 'sess_u', cwd: '/tmp', timestamp: '2026-04-23T00:00:00.000Z' } },
        { timestamp: '2026-04-23T00:00:00.050Z', type: 'turn_context', payload: { turn_id: 'turn_u', cwd: '/tmp', model: 'gpt-5.4' } },
        { timestamp: '2026-04-23T00:00:00.200Z', type: 'event_msg', payload: { type: 'task_started', turn_id: 'turn_u' } },
        { timestamp: '2026-04-23T00:00:00.400Z', type: 'response_item', payload: { type: 'function_call', name: 'spawn_agent', arguments: '{"subagent_type":"x"}', call_id: 'call_u_1' } },
        { timestamp: '2026-04-23T00:00:00.500Z', type: 'response_item', payload: { type: 'function_call_output', call_id: 'call_u_1', output: '{"agent_id":"agent_u"}' } },
        // no task_complete
      ];
      const file = path.join(tmp, 'session.jsonl');
      await writeFile(file, lines.map((l) => JSON.stringify(l)).join('\n') + '\n', 'utf8');
      const { turns, relationships, toolResultEvents } = await parseCodexSession(file);
      assert.equal(turns.length, 0);
      assert.equal(relationships.length, 0, 'root deferred until first task_complete');
      assert.equal(toolResultEvents.length, 0, 'tool result events deferred too');
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });
});

describe('parseCodexSessionIncremental execution graph dedup (#87)', () => {
  it('produces no duplicate (sessionId, toolUseId, eventIndex) tuples after re-parse', async () => {
    const file = path.join(FIXTURES, 'user-turn-blocks.jsonl');
    const a = await parseCodexSessionIncremental(file);
    const b = await parseCodexSessionIncremental(file);

    const key = (e: { sessionId: string; toolUseId: string; eventIndex: number }) =>
      `${e.sessionId}|${e.toolUseId}|${e.eventIndex}`;
    const aKeys = new Set(a.toolResultEvents.map(key));
    const bKeys = new Set(b.toolResultEvents.map(key));
    // Two independent passes against the same file produce identical
    // (and therefore deduplicable at the writer) tuples.
    assert.deepEqual([...aKeys].sort(), [...bKeys].sort());
  });

  it('does not double-emit relationships or events across resumed passes', async () => {
    const file = path.join(FIXTURES, 'user-turn-blocks.jsonl');
    const raw = await readFile(file, 'utf8');
    const lines = raw.split('\n');
    const cutIdx = lines.findIndex((l) =>
      l.includes('"task_complete"') && l.includes('"turn_utb_2"'),
    );
    assert.ok(cutIdx > 0);
    const cutoff = Buffer.byteLength(lines.slice(0, cutIdx + 1).join('\n') + '\n', 'utf8');

    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-eg-inc-'));
    try {
      const partialPath = path.join(tmp, 'partial.jsonl');
      await writeFile(partialPath, raw.slice(0, cutoff), 'utf8');
      const partial = await parseCodexSessionIncremental(partialPath);
      // Partial pass should have emitted: 1 root + the tool result events
      // for call_b1 / call_b2 / call_p1.
      const partialRoots = partial.relationships.filter((r) => r.relationshipType === 'root');
      assert.equal(partialRoots.length, 1);
      const partialEventIds = partial.toolResultEvents.map((e) => e.toolUseId).sort();
      assert.deepEqual(partialEventIds, ['call_b1', 'call_b2', 'call_p1']);

      const fullPath = path.join(tmp, 'full.jsonl');
      await writeFile(fullPath, raw, 'utf8');
      const resumed = await parseCodexSessionIncremental(fullPath, {
        startOffset: partial.endOffset,
        resume: partial.resume,
      });
      // Resumed pass must NOT re-emit the root, must NOT re-emit the
      // already-committed events, and must emit the tail event for call_b3.
      const resumedRoots = resumed.relationships.filter((r) => r.relationshipType === 'root');
      assert.equal(resumedRoots.length, 0, 'root row not duplicated across passes');
      const resumedEventIds = resumed.toolResultEvents.map((e) => e.toolUseId).sort();
      assert.deepEqual(resumedEventIds, ['call_b3']);

      // eventIndex must remain monotonic across the two passes.
      const indices = [
        ...partial.toolResultEvents.map((e) => e.eventIndex),
        ...resumed.toolResultEvents.map((e) => e.eventIndex),
      ];
      assert.equal(new Set(indices).size, indices.length, 'no duplicate eventIndex');
      assert.deepEqual(indices, [...indices].sort((a, b) => a - b));

      // Combined emission must equal the single-pass full parse (with
      // identical eventIndex values, which is the dedup key at the writer).
      const fullPass = await parseCodexSession(fullPath);
      const key = (e: { sessionId: string; toolUseId: string; eventIndex: number }) =>
        `${e.sessionId}|${e.toolUseId}|${e.eventIndex}`;
      const combined = [
        ...partial.toolResultEvents.map(key),
        ...resumed.toolResultEvents.map(key),
      ].sort();
      const fullKeys = fullPass.toolResultEvents.map(key).sort();
      assert.deepEqual(combined, fullKeys);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });
});

describe('parseCodexSession fidelity (issue #84)', () => {
  it('emits per-turn full-fidelity coverage when token_count is present', async () => {
    const { turns } = await parseCodexSession(path.join(FIXTURES, 'simple-turn.jsonl'));
    assert.equal(turns.length, 1);
    const f = turns[0]!.fidelity;
    assert.ok(f, 'fidelity is populated');
    assert.equal(f!.granularity, 'per-turn');
    // Full requires input + output + cacheRead + tool calls + tool results +
    // session relationships. With #87 the Codex parser now emits root /
    // subagent relationship rows, so a token-complete Codex turn satisfies
    // the FULL_REQUIRED matrix.
    assert.equal(f!.coverage.hasInputTokens, true);
    assert.equal(f!.coverage.hasOutputTokens, true);
    assert.equal(f!.coverage.hasReasoningTokens, true);
    assert.equal(f!.coverage.hasCacheReadTokens, true);
    assert.equal(f!.coverage.hasCacheCreateTokens, false);
    assert.equal(f!.coverage.hasToolCalls, true);
    assert.equal(f!.coverage.hasToolResultEvents, true);
    assert.equal(f!.coverage.hasSessionRelationships, true);
    assert.equal(f!.coverage.hasRawContent, true);
    assert.equal(f!.class, 'full');
  });

  it('reports class=partial when token_count is absent for a turn', async () => {
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-fid-no-tc-'));
    try {
      const lines = [
        {
          timestamp: '2026-04-22T00:00:00.000Z',
          type: 'session_meta',
          payload: { id: 'sess_fid_no_tc', cwd: '/tmp/proj', timestamp: '2026-04-22T00:00:00.000Z' },
        },
        {
          timestamp: '2026-04-22T00:00:00.050Z',
          type: 'turn_context',
          payload: { turn_id: 'turn_fid_no_tc', cwd: '/tmp/proj', model: 'gpt-5.4' },
        },
        {
          timestamp: '2026-04-22T00:00:00.200Z',
          type: 'event_msg',
          payload: { type: 'task_started', turn_id: 'turn_fid_no_tc' },
        },
        {
          timestamp: '2026-04-22T00:00:00.300Z',
          type: 'response_item',
          payload: {
            type: 'message',
            role: 'assistant',
            content: [{ type: 'output_text', text: 'ok' }],
          },
        },
        // intentionally no token_count between task_started and task_complete
        {
          timestamp: '2026-04-22T00:00:00.500Z',
          type: 'event_msg',
          payload: { type: 'task_complete', turn_id: 'turn_fid_no_tc' },
        },
      ];
      const file = path.join(tmp, 'no-tc.jsonl');
      await writeFile(file, lines.map((l) => JSON.stringify(l)).join('\n') + '\n', 'utf8');
      const { turns } = await parseCodexSession(file);
      assert.equal(turns.length, 1);
      const t = turns[0]!;
      const f = t.fidelity;
      assert.ok(f);
      assert.equal(f!.granularity, 'per-turn');
      assert.equal(f!.class, 'partial');
      // Numeric usage is still 0 (existing post-hoc default), but the coverage
      // flag is the honest signal — see issue #84.
      assert.equal(f!.coverage.hasInputTokens, false);
      assert.equal(f!.coverage.hasOutputTokens, false);
      assert.equal(f!.coverage.hasReasoningTokens, false);
      assert.equal(f!.coverage.hasCacheReadTokens, false);
      // Capability flags stay true even when this particular turn carries no
      // tool activity — the source *would* surface them.
      assert.equal(f!.coverage.hasToolCalls, true);
      assert.equal(f!.coverage.hasToolResultEvents, true);
      assert.equal(f!.coverage.hasRawContent, true);
      // usage numerics still default to 0
      assert.equal(t.usage.input, 0);
      assert.equal(t.usage.output, 0);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });

  it('keeps tool-result coverage true for turns with function_call + function_call_output', async () => {
    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-fid-tools-'));
    try {
      const lines = [
        {
          timestamp: '2026-04-22T00:00:00.000Z',
          type: 'session_meta',
          payload: { id: 'sess_fid_tools', cwd: '/tmp/proj', timestamp: '2026-04-22T00:00:00.000Z' },
        },
        {
          timestamp: '2026-04-22T00:00:00.050Z',
          type: 'turn_context',
          payload: { turn_id: 'turn_fid_tools', cwd: '/tmp/proj', model: 'gpt-5.4' },
        },
        {
          timestamp: '2026-04-22T00:00:00.200Z',
          type: 'event_msg',
          payload: { type: 'task_started', turn_id: 'turn_fid_tools' },
        },
        {
          timestamp: '2026-04-22T00:00:00.400Z',
          type: 'response_item',
          payload: {
            type: 'function_call',
            name: 'shell',
            arguments: '{"cmd":"ls"}',
            call_id: 'call_fid_1',
          },
        },
        {
          timestamp: '2026-04-22T00:00:00.500Z',
          type: 'response_item',
          payload: {
            type: 'function_call_output',
            call_id: 'call_fid_1',
            output: 'README.md\n',
          },
        },
        {
          timestamp: '2026-04-22T00:00:00.800Z',
          type: 'event_msg',
          payload: {
            type: 'token_count',
            info: {
              total_token_usage: {
                input_tokens: 100,
                cached_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 5,
              },
            },
          },
        },
        {
          timestamp: '2026-04-22T00:00:00.900Z',
          type: 'event_msg',
          payload: { type: 'task_complete', turn_id: 'turn_fid_tools' },
        },
      ];
      const file = path.join(tmp, 'tools.jsonl');
      await writeFile(file, lines.map((l) => JSON.stringify(l)).join('\n') + '\n', 'utf8');
      const { turns } = await parseCodexSession(file);
      assert.equal(turns.length, 1);
      const f = turns[0]!.fidelity;
      assert.ok(f);
      assert.equal(f!.coverage.hasToolCalls, true);
      assert.equal(f!.coverage.hasToolResultEvents, true);
      // Sanity: token_count was present, so usage flags are on.
      assert.equal(f!.coverage.hasInputTokens, true);
      assert.equal(f!.coverage.hasReasoningTokens, true);
      assert.equal(f!.class, 'full');
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });

  it('every turn carries fidelity (summarizeFidelity unknown bucket would be 0)', async () => {
    // Mirrors `summarizeFidelity`'s behavior over a real session: any turn
    // without a `fidelity` field would land in the `unknown` bucket. Asserting
    // that no Codex turn lacks fidelity is the equivalent guarantee without a
    // cross-package import.
    const { turns } = await parseCodexSession(path.join(FIXTURES, 'multi-turn.jsonl'));
    assert.ok(turns.length > 0);
    for (const t of turns) {
      assert.ok(t.fidelity, `turn ${t.messageId} carries fidelity`);
      assert.equal(t.fidelity!.granularity, 'per-turn');
    }
  });

  it('incremental parser also populates fidelity on emitted turns', async () => {
    const file = path.join(FIXTURES, 'multi-turn.jsonl');
    const { turns } = await parseCodexSessionIncremental(file);
    assert.ok(turns.length > 0);
    for (const t of turns) {
      assert.ok(t.fidelity, `incremental turn ${t.messageId} carries fidelity`);
      assert.equal(t.fidelity!.granularity, 'per-turn');
    }
  });
});

describe('parseCodexSessionIncremental user-turn dedup (issue #81)', () => {
  it('emits userTurns once across resumed incremental passes', async () => {
    const file = path.join(FIXTURES, 'user-turn-blocks.jsonl');
    const raw = await readFile(file, 'utf8');
    const lines = raw.split('\n');
    // Cut just after task_complete of turn_utb_2 (the line containing
    // task_complete is index 19 in 0-based after splitting, since lines 0-18
    // cover session_meta through turn 2's task_complete). Find it dynamically.
    const cutIdx = lines.findIndex((l) =>
      l.includes('"task_complete"') && l.includes('"turn_utb_2"'),
    );
    assert.ok(cutIdx > 0);
    const cutoff = Buffer.byteLength(lines.slice(0, cutIdx + 1).join('\n') + '\n', 'utf8');

    const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-codex-utb-inc-'));
    try {
      const partialPath = path.join(tmp, 'partial.jsonl');
      await writeFile(partialPath, raw.slice(0, cutoff), 'utf8');
      const partial = await parseCodexSessionIncremental(partialPath);
      // Pass 1 should have emitted: pre-turn-1 text user turn + between 1-2 user turn.
      const firstIds = partial.userTurns.map((u) => u.userUuid).sort();
      assert.equal(partial.userTurns.length, 2);
      assert.equal(partial.endOffset, cutoff);

      const fullPath = path.join(tmp, 'full.jsonl');
      await writeFile(fullPath, raw, 'utf8');
      const resumed = await parseCodexSessionIncremental(fullPath, {
        startOffset: partial.endOffset,
        resume: partial.resume,
      });
      const secondIds = resumed.userTurns.map((u) => u.userUuid).sort();

      // Pass 2 should have emitted exactly the between-2-3 user turn.
      assert.equal(resumed.userTurns.length, 1);
      const between23 = resumed.userTurns[0]!;
      assert.equal(between23.precedingMessageId, 'turn_utb_2');
      assert.equal(between23.followingMessageId, 'turn_utb_3');
      // Tool outputs from turn 2 (which are bytes BEFORE committedEndOffset)
      // must be carried via the resume state's userTurnSlot, not re-read.
      assert.equal(between23.blocks.length, 2);
      const failBlock = between23.blocks.find((b) => b.toolUseId === 'call_b2');
      assert.ok(failBlock);
      assert.equal(failBlock!.isError, true);

      // Combined emission must equal the single-pass full parse.
      const fullPass = await parseCodexSession(fullPath);
      const combined = [...firstIds, ...secondIds].sort();
      const fullIds = fullPass.userTurns.map((u) => u.userUuid).sort();
      assert.deepEqual(combined, fullIds);
      assert.equal(new Set(combined).size, combined.length, 'no double-emit');
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });
});
