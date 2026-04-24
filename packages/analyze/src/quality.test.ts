import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { ContentRecord, ToolCall, TurnRecord } from '@relayburn/reader';

import { computeOneShotRate, computeQuality, inferOutcome } from './quality.js';

function tc(id: string, name: string, opts: Partial<ToolCall> = {}): ToolCall {
  return { id, name, argsHash: `${name}:${id}`, ...opts };
}

function turn(overrides: Partial<TurnRecord> & { messageId: string; turnIndex: number }): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's',
    model: 'claude-sonnet-4-6',
    ts: '2026-04-20T00:00:00.000Z',
    usage: { input: 10, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
    toolCalls: [],
    retries: 0,
    hasEdits: false,
    ...overrides,
  };
}

const FIXED_NOW = Date.parse('2026-04-21T00:00:00.000Z');

describe('inferOutcome', () => {
  it('returns empty/low for a session with zero turns', () => {
    const o = inferOutcome('s', [], undefined, FIXED_NOW);
    assert.equal(o.outcome, 'unknown');
    assert.equal(o.confidence, 'low');
    assert.equal(o.reason, 'empty');
  });

  it('marks a single-exchange assistant-ended session as completed/medium', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, stopReason: 'tool_use' }),
      turn({ messageId: 'm2', turnIndex: 1, stopReason: 'end_turn' }),
    ];
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'completed');
    assert.equal(o.confidence, 'medium');
    assert.equal(o.reason, 'single-exchange');
  });

  it('marks a very-short session as unknown/low', () => {
    const turns = [turn({ messageId: 'm1', turnIndex: 0, stopReason: 'tool_use' })];
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'unknown');
    assert.equal(o.reason, 'too-short');
  });

  it('marks a still-active (recent) session as unknown/low with isRecent=true', () => {
    const now = Date.parse('2026-04-20T00:05:00.000Z');
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, ts: '2026-04-20T00:00:00.000Z', stopReason: 'end_turn' }),
      turn({ messageId: 'm2', turnIndex: 1, ts: '2026-04-20T00:01:00.000Z', stopReason: 'end_turn' }),
      turn({ messageId: 'm3', turnIndex: 2, ts: '2026-04-20T00:02:00.000Z', stopReason: 'end_turn' }),
    ];
    const o = inferOutcome('s', turns, undefined, now);
    assert.equal(o.outcome, 'unknown');
    assert.equal(o.isRecent, true);
    assert.equal(o.reason, 'recent');
  });

  it('marks a user-ended long session as abandoned/high', () => {
    const turns = Array.from({ length: 10 }, (_, i) =>
      turn({ messageId: `m${i}`, turnIndex: i, stopReason: i === 9 ? 'tool_use' : 'end_turn' }),
    );
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'abandoned');
    assert.equal(o.confidence, 'high');
    assert.equal(o.reason, 'user-ended-long');
  });

  it('marks a user-ended short-medium session as abandoned/medium', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, stopReason: 'end_turn' }),
      turn({ messageId: 'm2', turnIndex: 1, stopReason: 'end_turn' }),
      turn({ messageId: 'm3', turnIndex: 2, stopReason: 'tool_use' }),
    ];
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'abandoned');
    assert.equal(o.confidence, 'medium');
    assert.equal(o.reason, 'user-ended');
  });

  it('marks a trailing-failure-streak session as errored/medium', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, stopReason: 'end_turn' }),
      turn({
        messageId: 'm2',
        turnIndex: 1,
        stopReason: 'end_turn',
        toolCalls: [tc('u1', 'Bash', { isError: true })],
      }),
      turn({
        messageId: 'm3',
        turnIndex: 2,
        stopReason: 'end_turn',
        toolCalls: [tc('u2', 'Bash', { isError: true })],
      }),
      turn({
        messageId: 'm4',
        turnIndex: 3,
        stopReason: 'end_turn',
        toolCalls: [tc('u3', 'Bash', { isError: true })],
      }),
    ];
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'errored');
    assert.equal(o.reason, 'failure-streak');
  });

  it('marks an assistant-ended session as completed/medium by default', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, stopReason: 'end_turn' }),
      turn({ messageId: 'm2', turnIndex: 1, stopReason: 'end_turn' }),
      turn({ messageId: 'm3', turnIndex: 2, stopReason: 'end_turn' }),
    ];
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'completed');
    assert.equal(o.confidence, 'medium');
    assert.equal(o.reason, 'assistant-ended');
  });

  it('classifies sessions with no stopReason (e.g. Codex) as completed/low/unknown-ending', () => {
    // Codex parser never sets stopReason; without the fallback, the default
    // classifier would mark these as abandoned/medium (false negative).
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, source: 'codex' }),
      turn({ messageId: 'm2', turnIndex: 1, source: 'codex' }),
      turn({ messageId: 'm3', turnIndex: 2, source: 'codex' }),
    ];
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'completed');
    assert.equal(o.confidence, 'low');
    assert.equal(o.reason, 'unknown-ending');
  });

  it('still detects trailing failure streak for sources without stopReason', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, source: 'codex' }),
      turn({
        messageId: 'm2',
        turnIndex: 1,
        source: 'codex',
        toolCalls: [tc('u1', 'Bash', { isError: true })],
      }),
      turn({
        messageId: 'm3',
        turnIndex: 2,
        source: 'codex',
        toolCalls: [tc('u2', 'Bash', { isError: true })],
      }),
      turn({
        messageId: 'm4',
        turnIndex: 3,
        source: 'codex',
        toolCalls: [tc('u3', 'Bash', { isError: true })],
      }),
    ];
    const o = inferOutcome('s', turns, undefined, FIXED_NOW);
    assert.equal(o.outcome, 'errored');
    assert.equal(o.reason, 'failure-streak');
  });

  it('downgrades assistant-ended to completed/low when last assistant text has a give-up phrase', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, stopReason: 'end_turn' }),
      turn({ messageId: 'm2', turnIndex: 1, stopReason: 'end_turn' }),
      turn({ messageId: 'm3', turnIndex: 2, stopReason: 'end_turn' }),
    ];
    const content: ContentRecord[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's',
        messageId: 'm3',
        ts: '2026-04-20T00:00:00.000Z',
        role: 'assistant',
        kind: 'text',
        text: "I'm unable to access the file, so I will stop here.",
      },
    ];
    const map = new Map<string, ContentRecord[]>([['s', content]]);
    const o = inferOutcome('s', turns, map, FIXED_NOW);
    assert.equal(o.outcome, 'completed');
    assert.equal(o.confidence, 'low');
    assert.equal(o.reason, 'give-up');
  });
});

describe('computeOneShotRate', () => {
  it('counts edit turns with zero retries as one-shot', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, hasEdits: true, retries: 0 }),
      turn({ messageId: 'm2', turnIndex: 1, hasEdits: true, retries: 2 }),
      turn({ messageId: 'm3', turnIndex: 2, hasEdits: true, retries: 0 }),
      turn({ messageId: 'm4', turnIndex: 3, hasEdits: false, retries: 5 }), // non-edit, ignored
    ];
    const m = computeOneShotRate('s', turns);
    assert.equal(m.editTurns, 3);
    assert.equal(m.oneShotTurns, 2);
    assert.equal(m.oneShotRate, 2 / 3);
    assert.equal(m.totalRetries, 2);
  });

  it('returns undefined rate when there are no edit turns', () => {
    const turns = [turn({ messageId: 'm1', turnIndex: 0, hasEdits: false })];
    const m = computeOneShotRate('s', turns);
    assert.equal(m.editTurns, 0);
    assert.equal(m.oneShotRate, undefined);
  });

  it('excludes sidechain (subagent) turns from the denominator', () => {
    const turns = [
      turn({ messageId: 'm1', turnIndex: 0, hasEdits: true, retries: 0 }),
      turn({
        messageId: 'm2',
        turnIndex: 1,
        hasEdits: true,
        retries: 5,
        subagent: { isSidechain: true },
      }),
    ];
    const m = computeOneShotRate('s', turns);
    assert.equal(m.editTurns, 1);
    assert.equal(m.oneShotRate, 1);
  });
});

describe('computeQuality — pairing', () => {
  it('emits outcome + one-shot for each session in the input', () => {
    const turns = [
      turn({ messageId: 'a1', turnIndex: 0, sessionId: 'A', hasEdits: true, stopReason: 'end_turn' }),
      turn({ messageId: 'a2', turnIndex: 1, sessionId: 'A', hasEdits: true, stopReason: 'end_turn' }),
      turn({ messageId: 'a3', turnIndex: 2, sessionId: 'A', stopReason: 'end_turn' }),
      turn({ messageId: 'b1', turnIndex: 0, sessionId: 'B', stopReason: 'tool_use' }),
    ];
    const q = computeQuality(turns, { now: FIXED_NOW });
    const aOut = q.outcomes.find((o) => o.sessionId === 'A')!;
    const bOut = q.outcomes.find((o) => o.sessionId === 'B')!;
    assert.equal(aOut.outcome, 'completed');
    assert.equal(bOut.outcome, 'unknown'); // messageCount < 3
    assert.equal(q.oneShot.find((m) => m.sessionId === 'A')!.editTurns, 2);
    assert.equal(q.oneShot.find((m) => m.sessionId === 'B')!.editTurns, 0);
  });
});
