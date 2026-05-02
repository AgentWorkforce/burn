import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { ToolCall, TurnRecord } from '@relayburn/reader';

import {
  DEFAULT_REPLACED_TOOL_TOKEN_COST,
  estimateSavingsForToolCall,
  summarizeReplacementSavings,
} from './replacement-savings.js';

function turn(toolCalls: ToolCall[]): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's',
    messageId: 'm',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: {
      input: 0,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls,
  };
}

function call(name: string, extras: Partial<ToolCall> = {}): ToolCall {
  return { id: `${name}-1`, name, argsHash: 'h', ...extras };
}

describe('replacement-savings', () => {
  it('returns undefined for tool calls without any annotation', () => {
    const result = estimateSavingsForToolCall(call('Bash'));
    assert.equal(result, undefined);
  });

  it('estimates tokens saved using the average per-call cost across replaced tools', () => {
    const tc = call('relaywash__Search', {
      replacedTools: ['Glob', 'Grep', 'Read'],
      collapsedCalls: 9,
    });
    const est = estimateSavingsForToolCall(tc);
    assert.ok(est);
    const avg =
      (DEFAULT_REPLACED_TOOL_TOKEN_COST.Glob! +
        DEFAULT_REPLACED_TOOL_TOKEN_COST.Grep! +
        DEFAULT_REPLACED_TOOL_TOKEN_COST.Read!) /
      3;
    assert.equal(est!.collapsedCalls, 9);
    assert.deepEqual(est!.replacedTools, ['Glob', 'Grep', 'Read']);
    assert.equal(est!.estimatedTokensSaved, Math.round(9 * avg));
  });

  it('falls back to a per-call default when a replaced tool name is unknown', () => {
    const tc = call('relaywash__Custom', {
      replacedTools: ['UnknownTool'],
      collapsedCalls: 2,
    });
    const est = estimateSavingsForToolCall(tc);
    assert.ok(est);
    // 2 calls × fallback cost.
    assert.equal(est!.estimatedTokensSaved, 2 * 800);
  });

  it('treats replaces-without-collapsedCalls as one call per listed name', () => {
    const tc = call('relaywash__Search', { replacedTools: ['Read', 'Grep'] });
    const est = estimateSavingsForToolCall(tc);
    assert.ok(est);
    assert.equal(est!.collapsedCalls, 2);
  });

  it('aggregates savings across many turns and tool names', () => {
    const turns = [
      turn([
        call('relaywash__Search', { replacedTools: ['Glob', 'Grep', 'Read'], collapsedCalls: 9 }),
        call('Bash'),
      ]),
      turn([
        call('relaywash__Search', { replacedTools: ['Read'], collapsedCalls: 4 }),
      ]),
    ];
    const summary = summarizeReplacementSavings(turns);
    assert.equal(summary.calls, 2);
    assert.equal(summary.collapsedCalls, 13);
    assert.ok(summary.estimatedTokensSaved > 0);
    const search = summary.byTool.get('relaywash__Search');
    assert.ok(search);
    assert.equal(search!.calls, 2);
    assert.equal(search!.collapsedCalls, 13);
  });

  it('returns an empty summary when no turn carries an annotation', () => {
    const summary = summarizeReplacementSavings([turn([call('Bash')])]);
    assert.equal(summary.calls, 0);
    assert.equal(summary.collapsedCalls, 0);
    assert.equal(summary.estimatedTokensSaved, 0);
    assert.equal(summary.byTool.size, 0);
  });
});
