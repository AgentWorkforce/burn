import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { parseOpencodeSession, parseOpencodeSessionIncremental } from './opencode.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures', 'opencode');

function sessionFile(fixture: string, sessionId: string): string {
  return path.join(FIXTURES, fixture, 'storage', 'session', 'global', `${sessionId}.json`);
}

describe('parseOpencodeSession', () => {
  it('parses a simple one-turn session', async () => {
    const { turns } = await parseOpencodeSession(sessionFile('simple', 'ses_simple'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.equal(t.v, 1);
    assert.equal(t.source, 'opencode');
    assert.equal(t.sessionId, 'ses_simple');
    assert.equal(t.messageId, 'msg_simple_asst');
    assert.equal(t.turnIndex, 0);
    assert.equal(t.model, 'anthropic/claude-sonnet-4-5');
    assert.equal(t.project, '/tmp/project');
    assert.equal(t.ts, '2026-04-24T00:00:02.000Z');
    assert.equal(t.stopReason, 'end_turn');
    assert.deepEqual(t.usage, {
      input: 10,
      output: 5,
      reasoning: 0,
      cacheRead: 500,
      cacheCreate5m: 80,
      cacheCreate1h: 0,
    });
    assert.equal(t.toolCalls.length, 0);
    assert.equal(t.filesTouched, undefined);
    assert.equal(t.subagent, undefined);
  });

  it('extracts tool calls and filesTouched only for file tools', async () => {
    const { turns } = await parseOpencodeSession(sessionFile('with-tool', 'ses_tool'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.equal(t.toolCalls.length, 3);
    const [read, edit, bash] = t.toolCalls;
    assert.equal(read!.name, 'read');
    assert.equal(read!.target, '/src/a.ts');
    assert.equal(edit!.name, 'edit');
    assert.equal(edit!.target, '/src/b.ts');
    assert.equal(bash!.name, 'bash');
    assert.equal(bash!.target, 'ls -la');
    assert.deepEqual(t.filesTouched?.sort(), ['/src/a.ts', '/src/b.ts']);
    assert.equal(t.stopReason, 'tool-calls');
  });

  it('emits per-turn (not cumulative) usage across multiple turns', async () => {
    const { turns } = await parseOpencodeSession(sessionFile('multi-turn', 'ses_multi'));
    assert.equal(turns.length, 2);
    const [t1, t2] = turns;
    assert.equal(t1!.messageId, 'msg_multi_a1');
    assert.equal(t1!.turnIndex, 0);
    assert.equal(t1!.model, 'anthropic/claude-sonnet-4-5');
    assert.deepEqual(t1!.usage, {
      input: 5,
      output: 100,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 15000,
      cacheCreate1h: 0,
    });
    assert.equal(t1!.subagent, undefined);

    assert.equal(t2!.messageId, 'msg_multi_a2');
    assert.equal(t2!.turnIndex, 1);
    assert.equal(t2!.model, 'anthropic/claude-opus-4-5');
    assert.deepEqual(t2!.usage, {
      input: 5,
      output: 200,
      reasoning: 50,
      cacheRead: 15000,
      cacheCreate5m: 3000,
      cacheCreate1h: 0,
    });
    assert.equal(t2!.toolCalls.length, 1);
    assert.equal(t2!.toolCalls[0]!.name, 'bash');
    assert.equal(t2!.toolCalls[0]!.target, 'git status');
  });

  it('marks turns in a session with parentID as sidechain', async () => {
    const { turns } = await parseOpencodeSession(sessionFile('multi-turn', 'ses_child'));
    assert.equal(turns.length, 1);
    const t = turns[0]!;
    assert.ok(t.subagent);
    assert.equal(t.subagent!.isSidechain, true);
    assert.equal(t.model, 'anthropic/claude-haiku-4-5');
  });

  it('produces stable argsHash for identical tool inputs', async () => {
    const a = await parseOpencodeSession(sessionFile('with-tool', 'ses_tool'));
    const b = await parseOpencodeSession(sessionFile('with-tool', 'ses_tool'));
    assert.equal(a.turns[0]!.toolCalls[0]!.argsHash, b.turns[0]!.toolCalls[0]!.argsHash);
    assert.notEqual(a.turns[0]!.toolCalls[0]!.argsHash, a.turns[0]!.toolCalls[1]!.argsHash);
  });

  it('respects sessionPath option', async () => {
    const file = sessionFile('simple', 'ses_simple');
    const { turns } = await parseOpencodeSession(file, { sessionPath: file });
    assert.equal(turns[0]!.sessionPath, file);
  });

  it('classifies activity for opencode turns via aliased tool names', async () => {
    const { turns } = await parseOpencodeSession(sessionFile('with-tool', 'ses_tool'));
    const t = turns[0]!;
    // `edit` is aliased to Edit → hasEdits=true, defaults to 'coding'.
    assert.equal(t.hasEdits, true);
    assert.equal(t.activity, 'coding');
  });

  it('marks opencode turns with an errored tool part as debugging', async () => {
    const { mkdtemp, mkdir, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-oc-fail-'));
    try {
      const storage = path.join(tmp, 'storage');
      const sessionDir = path.join(storage, 'session', 'global');
      const msgDir = path.join(storage, 'message', 'ses_fail');
      const partAsstDir = path.join(storage, 'part', 'msg_fail_asst');
      const partUserDir = path.join(storage, 'part', 'msg_fail_user');
      await mkdir(sessionDir, { recursive: true });
      await mkdir(msgDir, { recursive: true });
      await mkdir(partAsstDir, { recursive: true });
      await mkdir(partUserDir, { recursive: true });
      await writeFile(
        path.join(sessionDir, 'ses_fail.json'),
        JSON.stringify({ id: 'ses_fail', directory: '/tmp/proj' }),
      );
      await writeFile(
        path.join(msgDir, 'msg_fail_user.json'),
        JSON.stringify({
          id: 'msg_fail_user',
          sessionID: 'ses_fail',
          role: 'user',
          time: { created: 1_776_988_000_000 },
        }),
      );
      await writeFile(
        path.join(partUserDir, 'prt_fail_user_1.json'),
        JSON.stringify({
          id: 'prt_fail_user_1',
          sessionID: 'ses_fail',
          messageID: 'msg_fail_user',
          type: 'text',
          text: 'please check why the build is broken',
        }),
      );
      await writeFile(
        path.join(msgDir, 'msg_fail_asst.json'),
        JSON.stringify({
          id: 'msg_fail_asst',
          sessionID: 'ses_fail',
          role: 'assistant',
          providerID: 'anthropic',
          modelID: 'claude-haiku-4-5',
          time: { created: 1_776_988_001_000 },
          path: { cwd: '/tmp/proj' },
          tokens: { input: 10, output: 20, cache: { read: 0, write: 0 } },
        }),
      );
      await writeFile(
        path.join(partAsstDir, 'prt_fail_asst_1.json'),
        JSON.stringify({
          id: 'prt_fail_asst_1',
          sessionID: 'ses_fail',
          messageID: 'msg_fail_asst',
          type: 'tool',
          callID: 'call_fail_bash',
          tool: 'bash',
          state: {
            status: 'completed',
            input: { command: 'npm run build' },
            output: 'command not found: foo',
            metadata: { exit: 1 },
          },
        }),
      );
      const file = path.join(sessionDir, 'ses_fail.json');
      const { turns } = await parseOpencodeSession(file);
      assert.equal(turns.length, 1);
      // Non-zero exit on a bash call flags hasFailedTool → debugging wins.
      assert.equal(turns[0]!.activity, 'debugging');
      assert.equal(turns[0]!.hasEdits, false);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  });
});

describe('parseOpencodeSessionIncremental', () => {
  it('returns all turns + seenMessageIds when seen is empty', async () => {
    const file = sessionFile('multi-turn', 'ses_multi');
    const r = await parseOpencodeSessionIncremental(file);
    assert.equal(r.turns.length, 2);
    assert.ok(r.seenMessageIds.has('msg_multi_a1'));
    assert.ok(r.seenMessageIds.has('msg_multi_a2'));
  });

  it('filters already-seen messageIds', async () => {
    const file = sessionFile('multi-turn', 'ses_multi');
    const r = await parseOpencodeSessionIncremental(file, {
      seenMessageIds: new Set(['msg_multi_a1']),
    });
    assert.equal(r.turns.length, 1);
    assert.equal(r.turns[0]!.messageId, 'msg_multi_a2');
    assert.ok(r.seenMessageIds.has('msg_multi_a1'));
    assert.ok(r.seenMessageIds.has('msg_multi_a2'));
  });

  it('yields zero turns when all ids already seen', async () => {
    const file = sessionFile('multi-turn', 'ses_multi');
    const r = await parseOpencodeSessionIncremental(file, {
      seenMessageIds: new Set(['msg_multi_a1', 'msg_multi_a2']),
    });
    assert.equal(r.turns.length, 0);
  });
});
