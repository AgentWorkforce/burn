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
