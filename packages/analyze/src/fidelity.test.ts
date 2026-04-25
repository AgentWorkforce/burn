import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { EMPTY_COVERAGE, makeFidelity } from '@relayburn/reader';
import type { Fidelity, TurnRecord } from '@relayburn/reader';

import {
  emptyFidelitySummary,
  hasMinimumFidelity,
  summarizeFidelity,
} from './fidelity.js';

function turn(fidelity?: Fidelity): Pick<TurnRecord, 'fidelity'> {
  // exactOptionalPropertyTypes refuses `{ fidelity: undefined }` for the
  // optional field — only construct the property when we have a value.
  if (fidelity === undefined) return {};
  return { fidelity };
}

const FULL_FIDELITY: Fidelity = makeFidelity('per-turn', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  hasOutputTokens: true,
  hasCacheReadTokens: true,
  hasToolCalls: true,
  hasToolResultEvents: true,
  hasSessionRelationships: true,
});

const PARTIAL_FIDELITY: Fidelity = makeFidelity('per-turn', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  // missing output / cache-read / tool-result events → "partial"
});

const AGGREGATE_FIDELITY: Fidelity = makeFidelity('per-session-aggregate', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  hasOutputTokens: true,
});

describe('summarizeFidelity', () => {
  it('returns the empty summary for an empty turn list', () => {
    const s = summarizeFidelity([]);
    assert.deepEqual(s, emptyFidelitySummary());
  });

  it('counts each turn into byClass and byGranularity', () => {
    const s = summarizeFidelity([
      turn(FULL_FIDELITY),
      turn(FULL_FIDELITY),
      turn(PARTIAL_FIDELITY),
      turn(AGGREGATE_FIDELITY),
    ]);
    assert.equal(s.total, 4);
    assert.equal(s.byClass.full, 2);
    assert.equal(s.byClass.partial, 1);
    assert.equal(s.byClass['aggregate-only'], 1);
    assert.equal(s.byGranularity['per-turn'], 3);
    assert.equal(s.byGranularity['per-session-aggregate'], 1);
    assert.equal(s.unknown, 0);
  });

  it('reports records without fidelity in the unknown bucket', () => {
    const s = summarizeFidelity([turn(), turn(FULL_FIDELITY), turn()]);
    assert.equal(s.total, 3);
    assert.equal(s.unknown, 2);
    assert.equal(s.byClass.full, 1);
    // Unknown records do not get classified or counted as missing — they're
    // an explicit "we don't know" rather than "we know it's incomplete".
    assert.equal(s.missingCoverage.hasOutputTokens, 0);
  });

  it('counts missing fields correctly across mixed fidelity', () => {
    const s = summarizeFidelity([
      turn(FULL_FIDELITY),
      turn(PARTIAL_FIDELITY), // missing output, cacheRead, toolResultEvents, sessionRelationships, etc.
    ]);
    // FULL has input+output+cacheRead+toolCalls+toolResultEvents+sessionRelationships;
    // PARTIAL has only input. So:
    //   hasOutputTokens missing on 1 turn (PARTIAL)
    //   hasCacheReadTokens missing on 1 turn (PARTIAL)
    //   hasToolResultEvents missing on 1 turn (PARTIAL)
    //   hasSessionRelationships missing on 1 turn (PARTIAL)
    assert.equal(s.missingCoverage.hasInputTokens, 0);
    assert.equal(s.missingCoverage.hasOutputTokens, 1);
    assert.equal(s.missingCoverage.hasCacheReadTokens, 1);
    assert.equal(s.missingCoverage.hasToolResultEvents, 1);
    assert.equal(s.missingCoverage.hasSessionRelationships, 1);
    // hasReasoningTokens missing on both (no source surfaces it in tests)
    assert.equal(s.missingCoverage.hasReasoningTokens, 2);
  });
});

describe('hasMinimumFidelity', () => {
  it('treats undefined fidelity as passing (backward compat)', () => {
    assert.equal(hasMinimumFidelity(undefined, 'full'), true);
    assert.equal(hasMinimumFidelity(undefined, 'usage-only'), true);
  });

  it('orders classes from cost-only up to full', () => {
    assert.equal(hasMinimumFidelity(FULL_FIDELITY, 'usage-only'), true);
    assert.equal(hasMinimumFidelity(FULL_FIDELITY, 'full'), true);
    assert.equal(hasMinimumFidelity(PARTIAL_FIDELITY, 'usage-only'), false);
    assert.equal(hasMinimumFidelity(PARTIAL_FIDELITY, 'full'), false);
    assert.equal(hasMinimumFidelity(AGGREGATE_FIDELITY, 'aggregate-only'), true);
    assert.equal(hasMinimumFidelity(AGGREGATE_FIDELITY, 'usage-only'), false);
  });
});
