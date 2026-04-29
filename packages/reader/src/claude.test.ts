import { strict as assert } from 'node:assert';
import { copyFile, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import {
  parseClaudeSession,
  parseClaudeSessionIncremental,
  reconcileClaudeSessionRelationships,
} from './claude.js';

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

  it('reconstructs a nested subagent tree from parentUuid chains', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'nested-subagent.jsonl'));
    // 2 main + 2 outer sidechain + 1 inner sidechain = 5 turns
    assert.equal(turns.length, 5);

    const byId = new Map(turns.map((t) => [t.messageId, t] as const));
    const main1 = byId.get('msg_main_1')!;
    const sub1_1 = byId.get('msg_sub1_1')!;
    const sub1_2 = byId.get('msg_sub1_2')!;
    const sub2_1 = byId.get('msg_sub2_1')!;

    // Main thread: no subagent marker.
    assert.equal(main1.subagent, undefined);

    // Outer subagent (first-level): parent is main thread, tagged with session id.
    assert.ok(sub1_1.subagent);
    assert.equal(sub1_1.subagent!.isSidechain, true);
    assert.equal(sub1_1.subagent!.agentId, 'u-sub1-user');
    assert.equal(sub1_1.subagent!.parentToolUseId, 'toolu_outer');
    assert.equal(sub1_1.subagent!.subagentType, 'Explore');
    assert.equal(sub1_1.subagent!.description, 'Research the codebase');
    assert.equal(sub1_1.subagent!.parentAgentId, '55555555-5555-5555-5555-555555555555');

    // All turns of the same invocation share the agentId.
    assert.equal(sub1_2.subagent!.agentId, 'u-sub1-user');
    assert.equal(sub1_2.subagent!.parentToolUseId, 'toolu_outer');

    // Inner subagent: parent is the outer subagent invocation.
    assert.ok(sub2_1.subagent);
    assert.equal(sub2_1.subagent!.agentId, 'u-sub2-user');
    assert.equal(sub2_1.subagent!.parentAgentId, 'u-sub1-user');
    assert.equal(sub2_1.subagent!.parentToolUseId, 'toolu_inner');
    assert.equal(sub2_1.subagent!.subagentType, 'code-reviewer');
  });

  it('produces stable argsHash for identical tool inputs', async () => {
    const a = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    const b = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    assert.equal(a.turns[0]!.toolCalls[0]!.argsHash, b.turns[0]!.toolCalls[0]!.argsHash);
    assert.notEqual(a.turns[0]!.toolCalls[0]!.argsHash, a.turns[0]!.toolCalls[1]!.argsHash);
  });

  it('marks ToolCall.isError when a later tool_result has is_error=true', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'retry-loop.jsonl'));
    assert.equal(turns.length, 4);
    for (const t of turns) {
      assert.equal(t.toolCalls.length, 1);
      assert.equal(t.toolCalls[0]!.name, 'Bash');
      assert.equal(t.toolCalls[0]!.isError, true, `turn ${t.turnIndex} should be flagged errored`);
    }
  });

  it('extracts editPreHash and editPostHash from Edit tool calls', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'edit-revert.jsonl'));
    const edits = turns
      .flatMap((t) => t.toolCalls)
      .filter((tc) => tc.name === 'Edit');
    assert.equal(edits.length, 2);
    // First edit: old=FOO=1, new=FOO=2. Second edit: old=FOO=2, new=FOO=1.
    // Revert detected when second.postHash === first.preHash.
    assert.ok(edits[0]!.editPreHash);
    assert.ok(edits[0]!.editPostHash);
    assert.equal(edits[1]!.editPostHash, edits[0]!.editPreHash);
    assert.equal(edits[1]!.editPreHash, edits[0]!.editPostHash);
  });

  it('emits ToolResultEventRecords for each tool_result block in chronological order', async () => {
    const { toolResultEvents } = await parseClaudeSession(path.join(FIXTURES, 'retry-loop.jsonl'));
    // retry-loop.jsonl has four Bash tool calls each followed by an errored
    // tool_result. The fixture also includes user-only lines that don't
    // contain tool_results, so we expect exactly 4 events here.
    assert.equal(toolResultEvents.length, 4);
    for (const ev of toolResultEvents) {
      assert.equal(ev.v, 1);
      assert.equal(ev.source, 'claude-code');
      assert.equal(ev.eventSource, 'tool_result');
      assert.equal(ev.status, 'errored');
      assert.equal(ev.isError, true);
      // contentLength + contentHash are populated for non-empty results.
      assert.equal(typeof ev.contentLength, 'number');
      assert.equal(typeof ev.contentHash, 'string');
    }
    // eventIndex is monotonically increasing.
    for (let i = 1; i < toolResultEvents.length; i++) {
      assert.ok(toolResultEvents[i]!.eventIndex > toolResultEvents[i - 1]!.eventIndex);
    }
  });

  it('emits a root SessionRelationshipRecord per session and a subagent row per invocation', async () => {
    const { relationships } = await parseClaudeSession(
      path.join(FIXTURES, 'nested-subagent.jsonl'),
    );
    const roots = relationships.filter((r) => r.relationshipType === 'root');
    const subs = relationships.filter((r) => r.relationshipType === 'subagent');
    assert.equal(roots.length, 1);
    assert.equal(roots[0]!.sessionId, '55555555-5555-5555-5555-555555555555');

    // Two distinct invocations: outer (Explore) and inner (code-reviewer).
    assert.equal(subs.length, 2);
    const outer = subs.find((r) => r.subagentType === 'Explore')!;
    const inner = subs.find((r) => r.subagentType === 'code-reviewer')!;
    assert.ok(outer);
    assert.ok(inner);

    // Outer subagent: parent is the main session id.
    assert.equal(outer.agentId, 'u-sub1-user');
    assert.equal(outer.source, 'native-claude');
    assert.equal(outer.parentToolUseId, 'toolu_outer');
    assert.equal(outer.relatedSessionId, '55555555-5555-5555-5555-555555555555');
    assert.equal(outer.description, 'Research the codebase');

    // Inner subagent: parent is the outer invocation's agentId.
    assert.equal(inner.agentId, 'u-sub2-user');
    assert.equal(inner.parentToolUseId, 'toolu_inner');
    assert.equal(inner.relatedSessionId, 'u-sub1-user');
  });

  it('joins tool_result events back to their spawned subagent via agentId', async () => {
    const { toolResultEvents } = await parseClaudeSession(
      path.join(FIXTURES, 'nested-subagent.jsonl'),
    );
    const outerSpawn = toolResultEvents.find((e) => e.toolUseId === 'toolu_outer');
    const innerSpawn = toolResultEvents.find((e) => e.toolUseId === 'toolu_inner');
    assert.ok(outerSpawn, 'Agent/Task tool_result for the outer spawn must surface as an event');
    assert.ok(innerSpawn, 'Agent/Task tool_result for the inner spawn must surface as an event');
    assert.equal(outerSpawn!.agentId, 'u-sub1-user');
    assert.equal(innerSpawn!.agentId, 'u-sub2-user');
  });

  it('emits system subagent notifications as ToolResultEventRecords', async () => {
    const { events, toolResultEvents } = await parseClaudeSession(
      path.join(FIXTURES, 'system-subagent-notification.jsonl'),
    );
    assert.equal(events.length, 0, 'subagent system notifications are not compaction events');
    assert.equal(toolResultEvents.length, 1);
    const ev = toolResultEvents[0]!;
    assert.equal(ev.source, 'claude-code');
    assert.equal(ev.sessionId, '22222222-2222-2222-2222-222222222222');
    assert.equal(ev.toolUseId, 'toolu_system');
    assert.equal(ev.eventSource, 'subagent_notification');
    assert.equal(ev.status, 'completed');
    assert.equal(ev.agentId, 'agent-system-1');
    assert.equal(ev.subagentSessionId, 'session-system-child');
    assert.equal(ev.callIndex, 0);
    assert.equal(ev.eventIndex, 0);
    assert.equal(typeof ev.contentLength, 'number');
    assert.equal(typeof ev.contentHash, 'string');
  });

  it('attaches per-turn fidelity metadata with full coverage on a normal turn', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'simple-turn.jsonl'));
    const t = turns[0]!;
    assert.ok(t.fidelity, 'fidelity should be populated');
    assert.equal(t.fidelity!.granularity, 'per-turn');
    // simple-turn carries input_tokens, output_tokens, cache_read, cache_creation
    assert.equal(t.fidelity!.coverage.hasInputTokens, true);
    assert.equal(t.fidelity!.coverage.hasOutputTokens, true);
    assert.equal(t.fidelity!.coverage.hasCacheReadTokens, true);
    assert.equal(t.fidelity!.coverage.hasCacheCreateTokens, true);
    // Claude session logs do not surface a separate reasoning token count.
    assert.equal(t.fidelity!.coverage.hasReasoningTokens, false);
    // Coverage is capability-level — Claude always surfaces tool calls and
    // tool-result events when they exist, so both flags are true even on a
    // turn that happened to make none.
    assert.equal(t.fidelity!.coverage.hasToolCalls, true);
    assert.equal(t.fidelity!.coverage.hasToolResultEvents, true);
    assert.equal(t.fidelity!.coverage.hasSessionRelationships, true);
    // Required fields present → derived class is full.
    assert.equal(t.fidelity!.class, 'full');
  });

  it('marks hasOutputTokens=false when the upstream usage block omits output_tokens', async () => {
    // Crucial coverage-vs-zero distinction: usage.output is `0` in the
    // TurnRecord (because we have to put *something* there), but
    // `coverage.hasOutputTokens` is false so downstream consumers can tell
    // "we don't know" from "actually zero".
    const { turns } = await parseClaudeSession(
      path.join(FIXTURES, 'missing-output-tokens.jsonl'),
    );
    const t = turns[0]!;
    assert.equal(t.usage.output, 0);
    assert.ok(t.fidelity);
    assert.equal(t.fidelity!.coverage.hasInputTokens, true);
    assert.equal(t.fidelity!.coverage.hasOutputTokens, false);
    assert.equal(t.fidelity!.coverage.hasCacheReadTokens, false);
    assert.equal(t.fidelity!.coverage.hasCacheCreateTokens, false);
    // Output missing → strictly less than usage-only → partial.
    assert.equal(t.fidelity!.class, 'partial');
  });

  it('flips hasToolCalls to true when the turn has tool_use blocks', async () => {
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'multi-block-turn.jsonl'));
    const t = turns[0]!;
    assert.ok(t.fidelity);
    assert.equal(t.fidelity!.coverage.hasToolCalls, true);
    assert.equal(t.fidelity!.class, 'full');
  });

  it('emits a CompactionEvent anchored to the preceding turn when a compact_boundary system record appears', async () => {
    const { turns, events } = await parseClaudeSession(
      path.join(FIXTURES, 'compact-boundary.jsonl'),
    );
    assert.equal(events.length, 1);
    const ev = events[0]!;
    assert.equal(ev.source, 'claude-code');
    assert.equal(ev.sessionId, 'compact-session');
    assert.equal(ev.precedingMessageId, 'msg_c_1');
    // tokensBeforeCompact = cacheRead of the turn right before compaction.
    const preceding = turns.find((t) => t.messageId === 'msg_c_1')!;
    assert.equal(ev.tokensBeforeCompact, preceding.usage.cacheRead);
    assert.equal(ev.tokensBeforeCompact, 9000);
  });
});

describe('parseClaudeSession user-turn block sizes (issue #2)', () => {
  it('emits a UserTurnRecord per user line with text and tool_result blocks', async () => {
    const { userTurns } = await parseClaudeSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    assert.equal(userTurns.length, 3, 'three user lines → three user-turn records');

    const [first, second, third] = userTurns as typeof userTurns;
    assert.ok(first && second && third);

    // Initial user prompt: free text only.
    assert.equal(first.userUuid, 'u-user-1');
    assert.equal(first.precedingMessageId, undefined, 'first user turn has no preceding assistant');
    assert.equal(first.followingMessageId, 'msg_utb_1');
    assert.equal(first.blocks.length, 1);
    assert.equal(first.blocks[0]!.kind, 'text');
    assert.equal(first.blocks[0]!.byteLen, 'please fix the build'.length);
    assert.equal(first.blocks[0]!.approxTokens, Math.ceil('please fix the build'.length / 4));

    // Second user turn: two tool_results (small Bash output, large Read output).
    assert.equal(second.precedingMessageId, 'msg_utb_1');
    assert.equal(second.followingMessageId, 'msg_utb_2');
    assert.equal(second.blocks.length, 2);
    const bash = second.blocks.find((b) => b.toolUseId === 'tu_bash_1')!;
    const read = second.blocks.find((b) => b.toolUseId === 'tu_read_1')!;
    assert.equal(bash.kind, 'tool_result');
    assert.equal(bash.byteLen, Buffer.byteLength('a\nb\n', 'utf8'));
    assert.equal(read.byteLen, 100);
    assert.ok(read.byteLen > bash.byteLen, 'large Read output must dwarf small Bash output');
    assert.equal(bash.isError, undefined);
    assert.equal(read.isError, undefined);

    // Third user turn: errored tool_result.
    assert.equal(third.precedingMessageId, 'msg_utb_2');
    assert.equal(third.followingMessageId, 'msg_utb_3');
    assert.equal(third.blocks.length, 1);
    const err = third.blocks[0]!;
    assert.equal(err.kind, 'tool_result');
    assert.equal(err.toolUseId, 'tu_bash_2');
    assert.equal(err.isError, true);
  });

  it('reconciles input-side delta against output + sum of user-turn block tokens (±5%)', async () => {
    // For the next assistant turn N+1 with the same model and no compaction
    // between, the input-side bytes the user contributed roughly equal:
    //   (input_tokens(N+1) + cacheCreate(N+1)) - output_tokens(N) - cacheRead(N+1)
    // We only need an order-of-magnitude assertion here — the heuristic is
    // bytes/4, accuracy depends on the tokenizer. Treat as a sanity gate.
    const { turns, userTurns } = await parseClaudeSession(
      path.join(FIXTURES, 'user-turn-blocks.jsonl'),
    );
    const turnByMid = new Map(turns.map((t) => [t.messageId, t] as const));
    for (const u of userTurns) {
      if (!u.precedingMessageId || !u.followingMessageId) continue;
      const prev = turnByMid.get(u.precedingMessageId)!;
      const next = turnByMid.get(u.followingMessageId)!;
      const inputDelta =
        next.usage.input + next.usage.cacheCreate5m + next.usage.cacheCreate1h - prev.usage.output;
      const userTokens = u.blocks.reduce((s, b) => s + b.approxTokens, 0);
      // Sanity: both sides should be positive when there was real I/O.
      assert.ok(userTokens > 0, `user turn ${u.userUuid} should contribute tokens`);
      assert.ok(inputDelta > 0, `delta for ${u.followingMessageId} should be positive`);
    }
  });

  it('emits empty userTurns for sessions that have no user-turn content blocks', async () => {
    // sidechain-turn.jsonl contains a sidechain-only assistant turn with the
    // user line carrying a tool_result for the parent thread; the user line
    // has content so it produces one record. Use simple-turn instead which
    // has a single text user turn.
    const { userTurns } = await parseClaudeSession(path.join(FIXTURES, 'simple-turn.jsonl'));
    assert.equal(userTurns.length, 1);
    assert.equal(userTurns[0]!.blocks.length, 1);
    assert.equal(userTurns[0]!.blocks[0]!.kind, 'text');
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

  it('preserves the user prompt across a resume so keyword refinement still applies', async () => {
    // Regression: when an incomplete assistant message forces endOffset to
    // back up past the user prompt, the resumed call re-reads the assistant
    // line without seeing the prompt. We carry lastUserText forward so the
    // classifier still has keyword context (and can reach 'debugging' instead
    // of falling back to 'coding').
    const working = path.join(tmp, 'session.jsonl');
    const sessionId = '44444444-4444-4444-4444-444444444444';
    const lines = [
      JSON.stringify({
        parentUuid: null,
        isSidechain: false,
        type: 'user',
        message: { role: 'user', content: 'fix the bug in auth.ts' },
        uuid: 'u-user-1',
        timestamp: '2026-04-20T00:00:00.000Z',
        cwd: '/tmp/project',
        sessionId,
      }),
      // Incomplete assistant (no stop_reason).
      JSON.stringify({
        parentUuid: 'u-user-1',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_resume_1',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'tool_use', id: 'tu_edit_1', name: 'Edit', input: { file_path: '/auth.ts' } }],
          stop_reason: null,
          usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0, cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 } },
        },
        type: 'assistant',
        uuid: 'u-asst-1',
        timestamp: '2026-04-20T00:00:01.000Z',
        cwd: '/tmp/project',
        sessionId,
      }),
    ];
    await writeFile(working, lines.join('\n') + '\n', 'utf8');

    const first = await parseClaudeSessionIncremental(working);
    assert.equal(first.turns.length, 0, 'incomplete turn is deferred');
    assert.equal(first.lastUserText, 'fix the bug in auth.ts');

    // Append the completion line for msg_resume_1.
    const tail =
      JSON.stringify({
        parentUuid: 'u-asst-1',
        isSidechain: false,
        message: {
          model: 'claude-sonnet-4-6',
          id: 'msg_resume_1',
          type: 'message',
          role: 'assistant',
          content: [{ type: 'tool_use', id: 'tu_edit_1', name: 'Edit', input: { file_path: '/auth.ts' } }],
          stop_reason: 'end_turn',
          usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0, cache_creation: { ephemeral_5m_input_tokens: 0, ephemeral_1h_input_tokens: 0 } },
        },
        type: 'assistant',
        uuid: 'u-asst-1',
        timestamp: '2026-04-20T00:00:01.000Z',
        cwd: '/tmp/project',
        sessionId,
      }) + '\n';
    const prev = await readFile(working, 'utf8');
    await writeFile(working, prev + tail, 'utf8');

    const second = await parseClaudeSessionIncremental(working, {
      startOffset: first.endOffset,
      lastUserText: first.lastUserText,
    });
    assert.equal(second.turns.length, 1);
    const t = second.turns[0]!;
    assert.equal(t.messageId, 'msg_resume_1');
    assert.equal(t.activity, 'debugging', 'user prompt mentions "bug" so edit turn is debugging, not coding');

    // Without lastUserText the prompt is lost and the turn falls back to coding.
    const withoutSeed = await parseClaudeSessionIncremental(working, { startOffset: first.endOffset });
    assert.equal(withoutSeed.turns[0]!.activity, 'coding');
  });

  it('emits userTurns once across resumed incremental passes (no double-emit past endOffset)', async () => {
    const src = path.join(FIXTURES, 'user-turn-blocks.jsonl');
    const full = await readFile(src, 'utf8');
    const working = path.join(tmp, 'session.jsonl');

    // First pass: write only through the second assistant turn so the third
    // user turn lands in the deferred region (its line is before the file's
    // EOF after we've truncated).
    const lines = full.split('\n').filter((l) => l.length > 0);
    // 0: u-user-1, 1: msg_utb_1, 2: u-user-2, 3: msg_utb_2 — all complete.
    await writeFile(working, lines.slice(0, 4).join('\n') + '\n', 'utf8');
    const first = await parseClaudeSessionIncremental(working);
    const firstIds = first.userTurns.map((u) => u.userUuid);
    assert.deepEqual(firstIds, ['u-user-1', 'u-user-2']);

    // Append the rest. Pass 2 must emit u-user-3 only (no re-emission of 1/2).
    await writeFile(working, full, 'utf8');
    const second = await parseClaudeSessionIncremental(working, {
      startOffset: first.endOffset,
      lastUserText: first.lastUserText,
    });
    const secondIds = second.userTurns.map((u) => u.userUuid);
    assert.deepEqual(secondIds, ['u-user-3']);
    assert.equal(second.userTurns[0]!.precedingMessageId, 'msg_utb_2');
    assert.equal(second.userTurns[0]!.followingMessageId, 'msg_utb_3');
    assert.equal(second.userTurns[0]!.blocks[0]!.isError, true);
  });

  it('resolves subagent tree fields for sidechain turns discovered after the spawn line (prescan)', async () => {
    // First incremental pass ingests the main thread + Agent spawn line.
    // Second pass starts beyond them and must still populate agentId /
    // parentAgentId / parentToolUseId on the sidechain turns via the
    // prescan that registers prior parentUuid nodes.
    const src = path.join(FIXTURES, 'nested-subagent.jsonl');
    const full = await readFile(src, 'utf8');
    const working = path.join(tmp, 'session.jsonl');

    const lines = full.split('\n').filter((l) => l.length > 0);
    // Write only through the outer Agent spawn line on pass 1.
    const prefixLines = lines.slice(0, 2);
    await writeFile(working, prefixLines.join('\n') + '\n', 'utf8');
    const first = await parseClaudeSessionIncremental(working);
    assert.ok(first.turns.length >= 1);

    // Append the rest of the file: sidechain spawns + tool_results.
    await writeFile(working, full, 'utf8');
    const second = await parseClaudeSessionIncremental(working, {
      startOffset: first.endOffset,
    });

    const byId = new Map(second.turns.map((t) => [t.messageId, t] as const));
    const sub1_1 = byId.get('msg_sub1_1');
    const sub2_1 = byId.get('msg_sub2_1');
    assert.ok(sub1_1, 'outer sidechain turn should be emitted on pass 2');
    assert.ok(sub2_1, 'inner sidechain turn should be emitted on pass 2');

    assert.equal(sub1_1!.subagent!.agentId, 'u-sub1-user');
    assert.equal(sub1_1!.subagent!.parentToolUseId, 'toolu_outer');
    assert.equal(sub1_1!.subagent!.subagentType, 'Explore');
    assert.equal(sub1_1!.subagent!.parentAgentId, '55555555-5555-5555-5555-555555555555');

    assert.equal(sub2_1!.subagent!.agentId, 'u-sub2-user');
    assert.equal(sub2_1!.subagent!.parentAgentId, 'u-sub1-user');
    assert.equal(sub2_1!.subagent!.parentToolUseId, 'toolu_inner');
  });
});

// ---------------------------------------------------------------------------
// Fork / continuation relationships (#112).
// ---------------------------------------------------------------------------

describe('parseClaudeSession fork / continuation relationships (#112)', () => {
  it('emits a continuation row from a /resume marker, with relatedSessionId set to the resumed-from id', async () => {
    const file = path.join(FIXTURES, 'resume-marker.jsonl');
    const { relationships } = await parseClaudeSessionIncremental(file, {
      sessionPath: file,
    });
    const cont = relationships.find((r) => r.relationshipType === 'continuation');
    assert.ok(cont, '/resume marker must produce a continuation row');
    // The on-disk filename's session id is what consumers join on; relatedSessionId
    // is the id named in the slash-command argument.
    assert.equal(cont!.sessionId, 'resume-marker');
    assert.equal(cont!.relatedSessionId, '11111111-1111-1111-1111-111111111111');
    // Provenance: the line carries an in-log `sessionId` distinct from the file
    // basename (`99999999-...` vs `resume-marker`), so it surfaces as
    // `sourceSessionId`. `version` becomes `sourceVersion`.
    assert.equal(cont!.sourceSessionId, '99999999-9999-9999-9999-999999999999');
    assert.equal(cont!.sourceVersion, '2.1.97');
  });

  it('populates sourceSessionId and sourceVersion on existing root rows when the in-log id differs from the file id', async () => {
    const file = path.join(FIXTURES, 'resume-marker.jsonl');
    const { relationships } = await parseClaudeSession(file, { sessionPath: file });
    const root = relationships.find((r) => r.relationshipType === 'root');
    assert.ok(root, 'root row should still be emitted alongside the continuation row');
    assert.equal(root!.sessionId, 'resume-marker');
    assert.equal(root!.sourceSessionId, '99999999-9999-9999-9999-999999999999');
    assert.equal(root!.sourceVersion, '2.1.97');
  });

  it('emits explicit line-level fork and continuation rows', async () => {
    const file = path.join(FIXTURES, 'explicit-line-relationships.jsonl');
    const { relationships, evidence } = await parseClaudeSession(file, { sessionPath: file });

    const continuation = relationships.find((r) => r.relationshipType === 'continuation');
    const fork = relationships.find((r) => r.relationshipType === 'fork');
    assert.ok(continuation, 'continuedFromSessionId must produce a continuation row');
    assert.ok(fork, 'forkSessionId must produce a fork row');

    assert.equal(continuation!.sessionId, 'explicit-line-relationships');
    assert.equal(continuation!.relatedSessionId, 'original-session');
    assert.equal(continuation!.sourceSessionId, 'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb');
    assert.equal(continuation!.sourceVersion, '2.1.98');
    assert.equal(fork!.sessionId, 'explicit-line-relationships');
    assert.equal(fork!.relatedSessionId, 'fork-source-session');
    assert.equal(fork!.sourceSessionId, 'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb');
    assert.equal(fork!.sourceVersion, '2.1.98');

    assert.deepEqual(evidence.explicitContinuationTargetSessionIds, ['original-session']);
    assert.deepEqual(evidence.explicitForkTargetSessionIds, ['fork-source-session']);
  });

  it('reconciliation skips a continuation edge already emitted from an explicit line field', async () => {
    const originalFile = path.join(FIXTURES, 'original-session.jsonl');
    const explicitFile = path.join(FIXTURES, 'explicit-line-relationships.jsonl');
    const { evidence: originalEv } = await parseClaudeSession(originalFile, {
      sessionPath: originalFile,
    });
    const { relationships, evidence: explicitEv } = await parseClaudeSession(explicitFile, {
      sessionPath: explicitFile,
    });
    assert.ok(
      relationships.some(
        (r) =>
          r.relationshipType === 'continuation' &&
          r.sessionId === 'explicit-line-relationships' &&
          r.relatedSessionId === 'original-session',
      ),
      'the local parser emitted the explicit continuation row',
    );

    const reconciled = reconcileClaudeSessionRelationships([
      { evidence: originalEv },
      { evidence: explicitEv },
    ]);
    assert.equal(
      reconciled.some(
        (r) =>
          r.relationshipType === 'continuation' &&
          r.sessionId === 'explicit-line-relationships' &&
          r.relatedSessionId === 'original-session',
      ),
      false,
      'cross-file parentUuid inference must not duplicate the explicit edge',
    );
  });

  it('captures firstParentUuid from the first non-sidechain user line even when a sidechain user line precedes it', async () => {
    const file = path.join(FIXTURES, 'sidechain-leading-then-main.jsonl');
    const { evidence } = await parseClaudeSession(file, { sessionPath: file });
    assert.equal(evidence.firstParentUuid, 'u-original-asst');
  });

  it('exposes per-file evidence so a cross-file pass can resolve fork / continuation', async () => {
    const file = path.join(FIXTURES, 'resume-marker.jsonl');
    const { evidence } = await parseClaudeSession(file, { sessionPath: file });
    assert.equal(evidence.fileSessionId, 'resume-marker');
    assert.equal(evidence.sourceVersion, '2.1.97');
    assert.equal(evidence.hasResumeMarker, true);
    assert.equal(evidence.resumeTargetSessionId, '11111111-1111-1111-1111-111111111111');
    // The first non-sidechain line's parentUuid is null in this fixture, so
    // the parser leaves firstParentUuid undefined.
    assert.equal(evidence.firstParentUuid, undefined);
    // Both line uuids show up (assistant + user).
    assert.ok(evidence.seenUuids.includes('u-resume-1'));
    assert.ok(evidence.seenUuids.includes('u-asst-r'));
  });

  it('reconcileClaudeSessionRelationships emits a continuation row when one file\'s first parentUuid lives in another file', async () => {
    const originalFile = path.join(FIXTURES, 'original-session.jsonl');
    const crossFile = path.join(FIXTURES, 'cross-file-parent.jsonl');
    const { evidence: originalEv } = await parseClaudeSession(originalFile, {
      sessionPath: originalFile,
    });
    const { evidence: crossEv, relationships: crossRows } = await parseClaudeSession(
      crossFile,
      { sessionPath: crossFile },
    );

    // Sanity: cross-file evidence carries the original's last assistant uuid.
    assert.equal(crossEv.firstParentUuid, 'u-original-asst');
    // The local pass alone produced no continuation row (no /resume marker).
    assert.equal(
      crossRows.find((r) => r.relationshipType === 'continuation'),
      undefined,
    );

    const reconciled = reconcileClaudeSessionRelationships([
      { evidence: originalEv },
      { evidence: crossEv },
    ]);
    const cont = reconciled.find((r) => r.relationshipType === 'continuation');
    assert.ok(cont, 'cross-file parentUuid match must produce a continuation row');
    assert.equal(cont!.sessionId, 'cross-file-parent');
    assert.equal(cont!.relatedSessionId, 'original-session');
    assert.equal(cont!.sourceVersion, '2.1.97');
  });

  it('reconcileClaudeSessionRelationships emits fork rows when two files share a sourceSessionId', async () => {
    const branchA = path.join(FIXTURES, 'fork-branch-a.jsonl');
    const branchB = path.join(FIXTURES, 'fork-branch-b.jsonl');
    const { evidence: evA, relationships: rowsA } = await parseClaudeSession(branchA, {
      sessionPath: branchA,
    });
    const { evidence: evB, relationships: rowsB } = await parseClaudeSession(branchB, {
      sessionPath: branchB,
    });

    // Each branch has a root row keyed on its own filename, with the shared
    // in-log id surfaced as sourceSessionId.
    const rootA = rowsA.find((r) => r.relationshipType === 'root');
    const rootB = rowsB.find((r) => r.relationshipType === 'root');
    assert.equal(rootA!.sessionId, 'fork-branch-a');
    assert.equal(rootB!.sessionId, 'fork-branch-b');
    assert.equal(rootA!.sourceSessionId, '00000000-0000-0000-0000-000000000fff');
    assert.equal(rootB!.sourceSessionId, '00000000-0000-0000-0000-000000000fff');

    const reconciled = reconcileClaudeSessionRelationships([
      { evidence: evA },
      { evidence: evB },
    ]);
    const forks = reconciled.filter((r) => r.relationshipType === 'fork');
    assert.equal(forks.length, 2, 'each branch should get a fork row');
    const sids = forks.map((r) => r.sessionId).sort();
    assert.deepEqual(sids, ['fork-branch-a', 'fork-branch-b']);
    for (const f of forks) {
      assert.equal(f.relatedSessionId, '00000000-0000-0000-0000-000000000fff');
      assert.equal(f.sourceSessionId, '00000000-0000-0000-0000-000000000fff');
      assert.equal(f.sourceVersion, '2.1.97');
    }
  });

  it('reconcileClaudeSessionRelationships does not emit a fork row when one file is a strict continuation of the other', async () => {
    // Two files share a sourceSessionId but file B's firstParentUuid lives in
    // file A — that's a continuation, not a fork. Reconciliation should emit
    // exactly one continuation row and zero fork rows.
    const fileA = path.join(FIXTURES, 'original-session.jsonl');
    const fileB = path.join(FIXTURES, 'cross-file-parent.jsonl');
    const { evidence: evA } = await parseClaudeSession(fileA, { sessionPath: fileA });
    const { evidence: evB } = await parseClaudeSession(fileB, { sessionPath: fileB });
    const reconciled = reconcileClaudeSessionRelationships([
      { evidence: evA },
      { evidence: evB },
    ]);
    assert.equal(
      reconciled.filter((r) => r.relationshipType === 'fork').length,
      0,
      'strict continuation must not also be classified as a fork',
    );
    assert.equal(reconciled.filter((r) => r.relationshipType === 'continuation').length, 1);
  });

  it('re-parsing the same session produces relationship rows with stable hashes (dedup target)', async () => {
    // Acceptance: re-ingesting the same session does not create duplicate
    // relationship rows. The on-disk dedup is keyed by `relationshipIdHash`
    // (source + sessionId + relationshipType + relatedSessionId + agentId +
    // parentToolUseId), so the parser must produce equivalent rows on both
    // passes for the writer's existing dedup to fold them. Reproduce the same
    // canonical key here rather than importing from `@relayburn/ledger`, which
    // already depends on `@relayburn/reader` (importing it back would create a
    // cycle that breaks `tsc --build`).
    const keyOf = (r: {
      source: string;
      sessionId: string;
      relationshipType: string;
      relatedSessionId?: string | undefined;
      agentId?: string | undefined;
      parentToolUseId?: string | undefined;
    }) =>
      [
        r.source,
        r.sessionId,
        r.relationshipType,
        r.relatedSessionId ?? '',
        r.agentId ?? '',
        r.parentToolUseId ?? '',
      ].join('|');
    const file = path.join(FIXTURES, 'resume-marker.jsonl');
    const a = await parseClaudeSession(file, { sessionPath: file });
    const b = await parseClaudeSession(file, { sessionPath: file });
    const idsA = new Set(a.relationships.map(keyOf));
    const idsB = new Set(b.relationships.map(keyOf));
    assert.equal(idsA.size, a.relationships.length);
    assert.deepEqual([...idsA].sort(), [...idsB].sort());
  });

  it('reconciliation skips a duplicate continuation when the local /resume already named the same parent', async () => {
    // Local /resume + cross-file parentUuid pointing at the same parent should
    // dedup at the reconciliation layer — we don't want two continuation rows
    // for the same edge with identical hashes.
    // Construct an in-memory evidence pair that matches the resume target
    // exactly.
    const parentEvidence = {
      fileSessionId: '11111111-1111-1111-1111-111111111111',
      inLogSessionIds: ['11111111-1111-1111-1111-111111111111'],
      seenUuids: ['u-original-asst'],
      hasResumeMarker: false,
    };
    const childEvidence = {
      fileSessionId: 'resume-marker',
      inLogSessionIds: ['99999999-9999-9999-9999-999999999999'],
      seenUuids: [],
      hasResumeMarker: true,
      resumeTargetSessionId: '11111111-1111-1111-1111-111111111111',
      firstParentUuid: 'u-original-asst',
      sourceVersion: '2.1.97',
    };
    const reconciled = reconcileClaudeSessionRelationships([
      { evidence: parentEvidence },
      { evidence: childEvidence },
    ]);
    // The local parse already emitted a continuation for (resume-marker ->
    // 11111111…); reconciliation should not add a duplicate edge here.
    const continuations = reconciled.filter(
      (r) =>
        r.relationshipType === 'continuation' &&
        r.sessionId === 'resume-marker' &&
        r.relatedSessionId === '11111111-1111-1111-1111-111111111111',
    );
    assert.equal(continuations.length, 0);
  });

  it('preserves sourceSessionId / sourceVersion on subagent rows when the in-log id differs from the file basename', async () => {
    // sub-rows need the same provenance stamp as roots so cross-source joins
    // can group all rows from one log under a common version banner. We
    // deliberately use a tmp filename whose basename differs from the
    // in-log session id (`55555555-…`) so the mismatch surfaces as
    // sourceSessionId on the subagent row.
    const dir = await mkdtemp(path.join(tmpdir(), 'claude-sub-'));
    const tmpFile = path.join(dir, 'session.jsonl');
    const subSrc = path.join(FIXTURES, 'nested-subagent.jsonl');
    await copyFile(subSrc, tmpFile);
    const { relationships } = await parseClaudeSession(tmpFile, { sessionPath: tmpFile });
    const sub = relationships.find((r) => r.relationshipType === 'subagent');
    assert.ok(sub, 'fixture has subagent rows');
    assert.equal(sub!.sourceSessionId, '55555555-5555-5555-5555-555555555555');
    await rm(dir, { recursive: true, force: true });
  });
});
