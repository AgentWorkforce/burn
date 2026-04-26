import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type {
  BashAggregation,
  FileAggregation,
  SessionWasteTotals,
  SubagentAggregation,
  WasteResult,
} from '@relayburn/analyze';

import { EMPTY_COVERAGE, makeFidelity } from '@relayburn/reader';

import { checkWasteFidelity, formatWasteReport, isAttributionDegraded } from './waste.js';

function session(
  id: string,
  method: 'sized' | 'even-split',
): SessionWasteTotals {
  return {
    sessionId: id,
    grandCost: 1,
    attributedCost: 0.5,
    unattributedCost: 0.5,
    attributionMethod: method,
  };
}

function makeResult(sessions: SessionWasteTotals[]): WasteResult {
  return {
    attributions: [],
    sessionTotals: sessions,
    grandTotal: 100,
    attributedTotal: 25,
    unattributedTotal: 75,
  };
}

const NO_FILES: FileAggregation[] = [];
const NO_BASH: BashAggregation[] = [];
const NO_SUBAGENTS: SubagentAggregation[] = [];

describe('isAttributionDegraded', () => {
  it('returns false when there are no sessions', () => {
    assert.equal(isAttributionDegraded(makeResult([])), false);
  });

  it('returns false when all sessions are sized', () => {
    const r = makeResult([session('a', 'sized'), session('b', 'sized')]);
    assert.equal(isAttributionDegraded(r), false);
  });

  it('returns false when even-split is below 50%', () => {
    const r = makeResult([
      session('a', 'even-split'),
      session('b', 'sized'),
      session('c', 'sized'),
    ]);
    assert.equal(isAttributionDegraded(r), false);
  });

  it('returns true at exactly 50% even-split', () => {
    const r = makeResult([
      session('a', 'even-split'),
      session('b', 'sized'),
    ]);
    assert.equal(isAttributionDegraded(r), true);
  });

  it('returns true when even-split dominates', () => {
    const r = makeResult([
      session('a', 'even-split'),
      session('b', 'even-split'),
      session('c', 'even-split'),
      session('d', 'sized'),
    ]);
    assert.equal(isAttributionDegraded(r), true);
  });

  it('honors a custom threshold', () => {
    // 1/3 even-split, threshold 0.25 → degraded
    const r = makeResult([
      session('a', 'even-split'),
      session('b', 'sized'),
      session('c', 'sized'),
    ]);
    assert.equal(isAttributionDegraded(r, 0.25), true);
    assert.equal(isAttributionDegraded(r, 0.5), false);
  });
});

describe('formatWasteReport', () => {
  it('renders no even-split note when all sessions are sized', () => {
    const result = makeResult([
      session('a', 'sized'),
      session('b', 'sized'),
    ]);
    const out = formatWasteReport({
      turnsAnalyzed: 100,
      result,
      files: NO_FILES,
      bashes: NO_BASH,
      subagents: NO_SUBAGENTS,
      limit: 10,
      degraded: false,
    });
    assert.doesNotMatch(out, /even-split/);
    assert.doesNotMatch(out, /attribution is degraded/);
    assert.doesNotMatch(out, /\(approximate\)/);
    // Headings render plain.
    assert.match(out, /Top files by cumulative cost\n/);
    assert.match(out, /Top Bash commands by cost\n/);
    assert.match(out, /Top subagent calls by cost\n/);
    // Original combined attributed/unattributed line.
    assert.match(out, /attributed to tool calls: \$25\.00/);
  });

  it('keeps the footer note when partial even-split (< 50%)', () => {
    const result = makeResult([
      session('a', 'even-split'),
      session('b', 'sized'),
      session('c', 'sized'),
    ]);
    const out = formatWasteReport({
      turnsAnalyzed: 100,
      result,
      files: NO_FILES,
      bashes: NO_BASH,
      subagents: NO_SUBAGENTS,
      limit: 10,
      degraded: false,
    });
    assert.match(out, /note: 1\/3 sessions used even-split/);
    assert.doesNotMatch(out, /attribution is degraded/);
    assert.doesNotMatch(out, /\(approximate\)/);
    assert.match(out, /attributed to tool calls: \$25\.00/);
  });

  it('promotes to a banner with (approximate) suffixes when degraded', () => {
    // 2 of 3 → 66.7% even-split
    const result = makeResult([
      session('a', 'even-split'),
      session('b', 'even-split'),
      session('c', 'sized'),
    ]);
    const out = formatWasteReport({
      turnsAnalyzed: 64117,
      result,
      files: NO_FILES,
      bashes: NO_BASH,
      subagents: NO_SUBAGENTS,
      limit: 10,
      degraded: true,
    });
    // Banner is emitted with the warning glyph and the right counts.
    assert.match(
      out,
      /⚠ attribution is degraded: 2 of 3 sessions \(66\.7%\) have no content/,
    );
    // Remediation pointer is present.
    assert.match(out, /burn rebuild --content/);
    assert.match(out, /burn content/);
    // Softened attributed line uses ≈ and the qualifier.
    assert.match(out, /attributed ≈ \$25\.00\s+\(approximate — see above\)/);
    // Unattributed line keeps its breakdown.
    assert.match(
      out,
      /unattributed \$75\.00\s+\(output, system overhead, untracked\)/,
    );
    // All three table headings are suffixed with "(approximate)".
    assert.match(out, /Top files by cumulative cost \(approximate\)/);
    assert.match(out, /Top Bash commands by cost \(approximate\)/);
    assert.match(out, /Top subagent calls by cost \(approximate\)/);
    // Old footer note must NOT appear when banner is shown.
    assert.doesNotMatch(out, /^note: \d+\/\d+ sessions used even-split/m);
  });

  it('formats large session counts with thousands separators in the banner', () => {
    const sessions: SessionWasteTotals[] = [];
    for (let i = 0; i < 39_587; i++) {
      sessions.push(session(`s${i}`, i < 39_486 ? 'even-split' : 'sized'));
    }
    const result = makeResult(sessions);
    const out = formatWasteReport({
      turnsAnalyzed: 64117,
      result,
      files: NO_FILES,
      bashes: NO_BASH,
      subagents: NO_SUBAGENTS,
      limit: 10,
      degraded: true,
    });
    assert.match(
      out,
      /⚠ attribution is degraded: 39,486 of 39,587 sessions \(99\.7%\)/,
    );
  });
});

describe('checkWasteFidelity', () => {
  it('rejects aggregate-only turns with explicit missing prerequisites', () => {
    const support = checkWasteFidelity([
      {
        fidelity: makeFidelity('per-session-aggregate', {
          ...EMPTY_COVERAGE,
          hasInputTokens: true,
          hasOutputTokens: true,
        }),
        toolCalls: [{ name: 'Bash' }],
      },
    ]);
    assert.equal(support.supported, false);
    assert.equal(support.unsupportedTurns, 1);
    assert.deepEqual(support.missingPrerequisites, [
      'content lengths',
      'per-turn usage',
      'tool calls',
      'tool result events',
    ]);
    assert.equal(support.unsupportedByClass['aggregate-only'], 1);
  });

  it('requires session relationships only when subagent calls are present', () => {
    const base = makeFidelity('per-turn', {
      ...EMPTY_COVERAGE,
      hasInputTokens: true,
      hasOutputTokens: true,
      hasToolCalls: true,
      hasToolResultEvents: true,
      hasRawContent: true,
    });
    assert.equal(
      checkWasteFidelity([{ fidelity: base, toolCalls: [{ name: 'Bash' }] }]).supported,
      true,
    );
    const withSubagent = checkWasteFidelity([
      { fidelity: base, toolCalls: [{ name: 'Agent' }] },
    ]);
    assert.equal(withSubagent.supported, false);
    assert.deepEqual(withSubagent.missingPrerequisites, ['session relationships']);
  });
});
