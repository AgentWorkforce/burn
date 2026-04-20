import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { parseClaudeSession } from './claude.js';

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
