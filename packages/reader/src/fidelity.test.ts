import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { EMPTY_COVERAGE, classifyFidelity, makeFidelity } from './fidelity.js';
import type { Coverage } from './types.js';

function withFlags(flags: Partial<Coverage>): Coverage {
  return { ...EMPTY_COVERAGE, ...flags };
}

describe('classifyFidelity', () => {
  it('returns cost-only when granularity says cost-only, regardless of coverage', () => {
    const allOn = withFlags({
      hasInputTokens: true,
      hasOutputTokens: true,
      hasCacheReadTokens: true,
      hasToolCalls: true,
      hasToolResultEvents: true,
      hasSessionRelationships: true,
    });
    assert.equal(classifyFidelity('cost-only', allOn), 'cost-only');
    assert.equal(classifyFidelity('cost-only', EMPTY_COVERAGE), 'cost-only');
  });

  it('returns aggregate-only when granularity is per-session-aggregate', () => {
    const cov = withFlags({
      hasInputTokens: true,
      hasOutputTokens: true,
    });
    assert.equal(classifyFidelity('per-session-aggregate', cov), 'aggregate-only');
  });

  it('returns full when per-turn and all required fields are present', () => {
    const cov = withFlags({
      hasInputTokens: true,
      hasOutputTokens: true,
      hasCacheReadTokens: true,
      hasToolCalls: true,
      hasToolResultEvents: true,
      hasSessionRelationships: true,
    });
    assert.equal(classifyFidelity('per-turn', cov), 'full');
  });

  it('returns usage-only when input/output present but tool fidelity is missing', () => {
    const cov = withFlags({
      hasInputTokens: true,
      hasOutputTokens: true,
      hasCacheReadTokens: true,
      hasSessionRelationships: true,
      // no tool calls, no tool result events
    });
    assert.equal(classifyFidelity('per-turn', cov), 'usage-only');
  });

  it('returns partial when output tokens are missing', () => {
    const cov = withFlags({
      hasInputTokens: true,
      // no output tokens — strictly less than usage-only
      hasCacheReadTokens: true,
      hasToolCalls: true,
      hasToolResultEvents: true,
      hasSessionRelationships: true,
    });
    assert.equal(classifyFidelity('per-turn', cov), 'partial');
  });

  it('per-message granularity with all required fields still classifies as full', () => {
    // FidelityClass tracks completeness, not whether per-turn vs per-message;
    // the granularity field is what carries that nuance for command gating.
    const cov = withFlags({
      hasInputTokens: true,
      hasOutputTokens: true,
      hasCacheReadTokens: true,
      hasToolCalls: true,
      hasToolResultEvents: true,
      hasSessionRelationships: true,
    });
    assert.equal(classifyFidelity('per-message', cov), 'full');
  });
});

describe('makeFidelity', () => {
  it('packs granularity, coverage, and derived class together', () => {
    const cov = withFlags({
      hasInputTokens: true,
      hasOutputTokens: true,
    });
    const f = makeFidelity('per-turn', cov);
    assert.equal(f.granularity, 'per-turn');
    assert.equal(f.coverage, cov);
    assert.equal(f.class, 'usage-only');
  });

  it('does not mutate the EMPTY_COVERAGE constant', () => {
    const before = { ...EMPTY_COVERAGE };
    makeFidelity('per-turn', withFlags({ hasInputTokens: true }));
    assert.deepEqual(EMPTY_COVERAGE, before);
  });
});
