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
    assert.ok(t.fidelity);
    assert.equal(t.fidelity!.granularity, 'per-message');
    assert.equal(t.fidelity!.coverage.hasInputTokens, true);
    assert.equal(t.fidelity!.coverage.hasOutputTokens, true);
    assert.equal(t.fidelity!.coverage.hasReasoningTokens, true);
    assert.equal(t.fidelity!.coverage.hasCacheReadTokens, true);
    assert.equal(t.fidelity!.coverage.hasCacheCreateTokens, true);
    assert.equal(t.fidelity!.coverage.hasToolCalls, true);
    assert.equal(t.fidelity!.coverage.hasToolResultEvents, true);
    assert.equal(t.fidelity!.coverage.hasSessionRelationships, false);
    assert.equal(t.fidelity!.coverage.hasRawContent, true);
    assert.equal(t.fidelity!.class, 'usage-only');
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

  it('distinguishes missing token fields from reported zero token fields', async () => {
    const { mkdtemp, mkdir, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-oc-fidelity-'));
    try {
      const storage = path.join(tmp, 'storage');
      const sessionDir = path.join(storage, 'session', 'global');
      const msgDir = path.join(storage, 'message', 'ses_fidelity');
      await mkdir(sessionDir, { recursive: true });
      await mkdir(msgDir, { recursive: true });
      await writeFile(
        path.join(sessionDir, 'ses_fidelity.json'),
        JSON.stringify({ id: 'ses_fidelity', directory: '/tmp/proj' }),
      );
      await writeFile(
        path.join(msgDir, 'msg_fidelity_asst.json'),
        JSON.stringify({
          id: 'msg_fidelity_asst',
          sessionID: 'ses_fidelity',
          role: 'assistant',
          providerID: 'anthropic',
          modelID: 'claude-haiku-4-5',
          time: { created: 1_776_988_001_000 },
          path: { cwd: '/tmp/proj' },
          tokens: { input: 0, output: 0, cache: { read: 0 } },
        }),
      );
      const { turns } = await parseOpencodeSession(path.join(sessionDir, 'ses_fidelity.json'));
      assert.equal(turns.length, 1);
      const f = turns[0]!.fidelity!;
      assert.equal(turns[0]!.usage.input, 0);
      assert.equal(turns[0]!.usage.output, 0);
      assert.equal(f.coverage.hasInputTokens, true, 'input: 0 is known zero');
      assert.equal(f.coverage.hasOutputTokens, true, 'output: 0 is known zero');
      assert.equal(f.coverage.hasCacheReadTokens, true, 'cache.read: 0 is known zero');
      assert.equal(f.coverage.hasReasoningTokens, false, 'missing reasoning is unknown');
      assert.equal(f.coverage.hasCacheCreateTokens, false, 'missing cache.write is unknown');
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

describe('parseOpencodeSession content capture', () => {
  async function withFixture<T>(body: (file: string) => Promise<T>): Promise<T> {
    const { mkdtemp, mkdir, writeFile, rm } = await import('node:fs/promises');
    const { tmpdir } = await import('node:os');
    const tmp = await mkdtemp(path.join(tmpdir(), 'burn-oc-content-'));
    try {
      const storage = path.join(tmp, 'storage');
      const sessionDir = path.join(storage, 'session', 'global');
      const msgDir = path.join(storage, 'message', 'ses_content');
      const partAsstDir = path.join(storage, 'part', 'msg_content_asst');
      const partUserDir = path.join(storage, 'part', 'msg_content_user');
      await mkdir(sessionDir, { recursive: true });
      await mkdir(msgDir, { recursive: true });
      await mkdir(partAsstDir, { recursive: true });
      await mkdir(partUserDir, { recursive: true });
      await writeFile(
        path.join(sessionDir, 'ses_content.json'),
        JSON.stringify({ id: 'ses_content', directory: '/tmp/proj' }),
      );
      await writeFile(
        path.join(msgDir, 'msg_content_user.json'),
        JSON.stringify({
          id: 'msg_content_user',
          sessionID: 'ses_content',
          role: 'user',
          time: { created: 1_776_988_000_000 },
        }),
      );
      await writeFile(
        path.join(partUserDir, 'prt_user_a.json'),
        JSON.stringify({
          id: 'prt_user_a',
          sessionID: 'ses_content',
          messageID: 'msg_content_user',
          type: 'text',
          text: 'run tests',
        }),
      );
      // Synthetic user prompts (agent-mode nudges) must not appear in content.
      await writeFile(
        path.join(partUserDir, 'prt_user_b.json'),
        JSON.stringify({
          id: 'prt_user_b',
          sessionID: 'ses_content',
          messageID: 'msg_content_user',
          type: 'text',
          text: '<synthetic nudge>',
          synthetic: true,
        }),
      );
      await writeFile(
        path.join(msgDir, 'msg_content_asst.json'),
        JSON.stringify({
          id: 'msg_content_asst',
          sessionID: 'ses_content',
          role: 'assistant',
          providerID: 'anthropic',
          modelID: 'claude-sonnet-4-6',
          time: { created: 1_776_988_001_000 },
          path: { cwd: '/tmp/proj' },
          tokens: { input: 100, output: 20, cache: { read: 0, write: 0 } },
        }),
      );
      await writeFile(
        path.join(partAsstDir, 'prt_asst_a.json'),
        JSON.stringify({
          id: 'prt_asst_a',
          sessionID: 'ses_content',
          messageID: 'msg_content_asst',
          type: 'text',
          text: 'running now.',
        }),
      );
      await writeFile(
        path.join(partAsstDir, 'prt_asst_b.json'),
        JSON.stringify({
          id: 'prt_asst_b',
          sessionID: 'ses_content',
          messageID: 'msg_content_asst',
          type: 'tool',
          callID: 'call_oc_bash',
          tool: 'bash',
          state: {
            status: 'completed',
            input: { command: 'npm test' },
            output: '10 passed',
            metadata: { exit: 0 },
          },
        }),
      );
      await writeFile(
        path.join(partAsstDir, 'prt_asst_c.json'),
        JSON.stringify({
          id: 'prt_asst_c',
          sessionID: 'ses_content',
          messageID: 'msg_content_asst',
          type: 'tool',
          callID: 'call_oc_fail',
          tool: 'bash',
          state: {
            status: 'completed',
            input: { command: 'lint' },
            output: 'ERR',
            metadata: { exit: 2 },
          },
        }),
      );
      const file = path.join(sessionDir, 'ses_content.json');
      return await body(file);
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  }

  it('returns empty content when contentMode is off (default)', async () => {
    await withFixture(async (file) => {
      const { content } = await parseOpencodeSession(file);
      assert.deepEqual(content, []);
    });
  });

  it('returns empty content when contentMode is hash-only', async () => {
    await withFixture(async (file) => {
      const { content } = await parseOpencodeSession(file, { contentMode: 'hash-only' });
      assert.deepEqual(content, []);
    });
  });

  it('emits tool_result for each tool part, keyed by callID', async () => {
    await withFixture(async (file) => {
      const { turns, content } = await parseOpencodeSession(file, { contentMode: 'full' });
      assert.equal(turns.length, 1);
      const toolResults = content.filter((c) => c.kind === 'tool_result');
      assert.equal(toolResults.length, 2);
      const byId = new Map(toolResults.map((t) => [t.toolResult!.toolUseId, t]));
      assert.equal(byId.get('call_oc_bash')!.toolResult!.content, '10 passed');
      assert.equal(byId.get('call_oc_fail')!.toolResult!.content, 'ERR');
      assert.equal(byId.get('call_oc_fail')!.toolResult!.isError, true, 'exit!=0 flags isError');
      assert.equal(byId.get('call_oc_bash')!.toolResult!.isError, undefined);
      const turnToolIds = new Set(turns[0]!.toolCalls.map((tc) => tc.id));
      assert.ok(turnToolIds.has('call_oc_bash'));
      assert.ok(turnToolIds.has('call_oc_fail'));
    });
  });

  it('captures user text (skipping synthetic) and assistant text + tool_use', async () => {
    await withFixture(async (file) => {
      const { content } = await parseOpencodeSession(file, { contentMode: 'full' });
      const userTexts = content.filter((c) => c.role === 'user' && c.kind === 'text').map((c) => c.text);
      assert.deepEqual(userTexts, ['run tests']);
      const asstText = content.find((c) => c.role === 'assistant' && c.kind === 'text');
      assert.equal(asstText!.text, 'running now.');
      const toolUses = content.filter((c) => c.kind === 'tool_use');
      assert.equal(toolUses.length, 2);
    });
  });
});

describe('parseOpencodeSession user-turn block sizes (issue #86)', () => {
  it('emits one UserTurnRecord per gap between assistant turns', async () => {
    const file = sessionFile('user-turn-blocks', 'ses_utb');
    const { turns, userTurns } = await parseOpencodeSession(file);
    assert.equal(turns.length, 2);
    // Two gaps with content: pre-a1 (user text) and a1→a2 (tool outputs + user text).
    assert.equal(userTurns.length, 2);

    for (const u of userTurns) {
      assert.equal(u.v, 1);
      assert.equal(u.source, 'opencode');
      assert.equal(u.sessionId, 'ses_utb');
      assert.equal(typeof u.userUuid, 'string');
      assert.ok(u.userUuid.length > 0);
      assert.ok(u.ts.length > 0);
      assert.ok(u.blocks.length >= 1);
    }

    const [pre, between] = userTurns;
    // Pre-a1 user turn: free-text only, no preceding assistant.
    assert.equal(pre!.precedingMessageId, undefined);
    assert.equal(pre!.followingMessageId, 'msg_utb_a1');
    assert.equal(pre!.userUuid, 'msg_utb_u1', 'user message id is the natural userUuid');
    assert.equal(pre!.blocks.length, 1);
    assert.equal(pre!.blocks[0]!.kind, 'text');
    assert.equal(pre!.blocks[0]!.byteLen, Buffer.byteLength('fix the build', 'utf8'));
    assert.equal(pre!.blocks[0]!.approxTokens, Math.ceil(13 / 4));

    // a1 → a2 gap: tool outputs from a1's parts + user text from u2.
    assert.equal(between!.precedingMessageId, 'msg_utb_a1');
    assert.equal(between!.followingMessageId, 'msg_utb_a2');
    assert.equal(between!.userUuid, 'msg_utb_u2');
    assert.equal(between!.blocks.length, 3);

    const okBlock = between!.blocks.find((b) => b.toolUseId === 'call_b1');
    assert.ok(okBlock);
    assert.equal(okBlock!.kind, 'tool_result');
    assert.equal(okBlock!.byteLen, Buffer.byteLength('ok\n', 'utf8'));
    assert.equal(okBlock!.approxTokens, Math.ceil(3 / 4));
    assert.equal(okBlock!.isError, undefined);

    const failBlock = between!.blocks.find((b) => b.toolUseId === 'call_fail');
    assert.ok(failBlock);
    assert.equal(failBlock!.kind, 'tool_result');
    assert.equal(failBlock!.byteLen, Buffer.byteLength('ERROR: tests failed', 'utf8'));
    assert.equal(failBlock!.approxTokens, Math.ceil(19 / 4));
    assert.equal(failBlock!.isError, true, 'exit!=0 surfaces isError on the block');

    const txtBlock = between!.blocks.find((b) => b.kind === 'text');
    assert.ok(txtBlock);
    assert.equal(txtBlock!.byteLen, Buffer.byteLength('now run tests', 'utf8'));
  });

  it('reconciles input + cacheWrite delta against user-turn block tokens', async () => {
    // Per issue #86: `(input + cacheWrite) - output(prev)` on consecutive
    // assistant messages should be positive and the same order of magnitude
    // as the sum of approxTokens in the user turn between them. OpenCode
    // reports per-message tokens (not cumulative), so the math is direct.
    const file = sessionFile('user-turn-blocks', 'ses_utb');
    const { turns, userTurns } = await parseOpencodeSession(file);
    assert.equal(turns.length, 2);
    const between = userTurns.find((u) => u.followingMessageId === 'msg_utb_a2');
    assert.ok(between);
    const userTurnTokens = between!.blocks.reduce((s, b) => s + b.approxTokens, 0);
    const prev = turns[0]!;
    const cur = turns[1]!;
    // OpenCode usage maps cache.write → cacheCreate5m.
    const lhs = cur.usage.input + cur.usage.cacheCreate5m - prev.usage.output;
    assert.ok(lhs > 0, '(input + cacheWrite)(N+1) - output(N) should be positive');
    assert.equal(lhs, userTurnTokens, 'fixture is engineered for an exact reconciliation match');
  });

  it('emits empty userTurns for a session with no measurable user-side blocks', async () => {
    // The bundled multi-turn fixture has user messages with no part files
    // (no user text on disk), a1 has only step-finish (no tool parts), and
    // a2's tool outputs would attribute to a (non-existent) a3 gap. So no
    // gap on this session has any block to emit.
    const file = sessionFile('multi-turn', 'ses_multi');
    const { userTurns } = await parseOpencodeSession(file);
    assert.deepEqual(userTurns, []);
  });

  it('does not double-emit user turns across resumed incremental passes', async () => {
    // Pass 1 sees only a1 (pre-a1 user turn). Pass 2 sees a2 (a1→a2 user turn).
    // The seenMessageIds dedup prevents re-processing a1; the user turn between
    // a1 and a2 is built fresh on pass 2 by re-reading a1's tool parts.
    const file = sessionFile('user-turn-blocks', 'ses_utb');
    const first = await parseOpencodeSessionIncremental(file, {
      seenMessageIds: new Set(),
    });
    // Without filtering, both user turns are emitted in one pass.
    assert.equal(first.userTurns.length, 2);

    // Simulate a resumed pass: pass 1 processed a1, pass 2 picks up a2.
    const seenAfterPass1 = new Set(['msg_utb_a1']);
    const resumed = await parseOpencodeSessionIncremental(file, {
      seenMessageIds: seenAfterPass1,
    });
    assert.equal(resumed.turns.length, 1, 'only the new assistant is processed');
    assert.equal(resumed.turns[0]!.messageId, 'msg_utb_a2');
    // The resumed pass emits the a1→a2 user turn (built from a1's parts on
    // disk + u2's text), with no double-emit of pre-a1.
    assert.equal(resumed.userTurns.length, 1);
    const u = resumed.userTurns[0]!;
    assert.equal(u.precedingMessageId, 'msg_utb_a1');
    assert.equal(u.followingMessageId, 'msg_utb_a2');
    assert.equal(u.blocks.length, 3, 'tool outputs + inter-turn text are all present');
  });
});
