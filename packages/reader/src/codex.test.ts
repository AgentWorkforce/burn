import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { parseCodexSession } from './codex.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures', 'codex');

describe('parseCodexSession', () => {
  it('parses a simple one-turn session', async () => {
    const turns = await parseCodexSession(path.join(FIXTURES, 'simple-turn.jsonl'));
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
    const turns = await parseCodexSession(path.join(FIXTURES, 'with-tool-call.jsonl'));
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
    const turns = await parseCodexSession(path.join(FIXTURES, 'multi-turn.jsonl'));
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
    assert.equal(a[0]!.toolCalls[0]!.argsHash, b[0]!.toolCalls[0]!.argsHash);
    assert.notEqual(a[0]!.toolCalls[0]!.argsHash, a[0]!.toolCalls[1]!.argsHash);
  });

  it('respects sessionPath option', async () => {
    const file = path.join(FIXTURES, 'simple-turn.jsonl');
    const turns = await parseCodexSession(file, { sessionPath: file });
    assert.equal(turns[0]!.sessionPath, file);
  });
});
