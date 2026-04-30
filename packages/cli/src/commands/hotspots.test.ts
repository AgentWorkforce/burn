import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { loadBuiltinPricing } from '@relayburn/analyze';
import type {
  BashAggregation,
  FileAggregation,
  SessionTotals,
  SubagentAggregation,
  HotspotsResult,
} from '@relayburn/analyze';
import type { EnrichedTurn } from '@relayburn/ledger';
import type { Coverage, Fidelity, SourceKind, ToolResultEventRecord } from '@relayburn/reader';

import {
  ATTRIBUTION_REQUIRED,
  PATTERN_REQUIRED,
  describeExcluded,
  fmtCoverageKey,
  formatCoverageNotice,
  formatHotspotsReport,
  isAttributionDegraded,
  renderSourcesClause,
  resolvePatternSelection,
  runPatternsMode,
  runHotspotsAttribution,
  turnPassesCoverage,
  type HotspotsAttributionDeps,
} from './hotspots.js';
import type { ParsedArgs } from '../args.js';

function session(
  id: string,
  method: 'sized' | 'even-split',
): SessionTotals {
  return {
    sessionId: id,
    grandCost: 1,
    attributedCost: 0.5,
    unattributedCost: 0.5,
    attributionMethod: method,
  };
}

function makeResult(sessions: SessionTotals[]): HotspotsResult {
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

describe('formatHotspotsReport', () => {
  it('renders no even-split note when all sessions are sized', () => {
    const result = makeResult([
      session('a', 'sized'),
      session('b', 'sized'),
    ]);
    const out = formatHotspotsReport({
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
    const out = formatHotspotsReport({
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
    const out = formatHotspotsReport({
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
      /⚠ attribution is degraded: 2 of 3 sessions \(66\.7%\) have no sized/,
    );
    // Remediation pointer is present.
    assert.match(out, /burn state rebuild content/);
    assert.match(out, /burn state/);
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
    const sessions: SessionTotals[] = [];
    for (let i = 0; i < 39_587; i++) {
      sessions.push(session(`s${i}`, i < 39_486 ? 'even-split' : 'sized'));
    }
    const result = makeResult(sessions);
    const out = formatHotspotsReport({
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

// ---------------------------------------------------------------------------
// Fidelity-gating helpers (#100)

function fullCoverage(): Coverage {
  return {
    hasInputTokens: true,
    hasOutputTokens: true,
    hasReasoningTokens: true,
    hasCacheReadTokens: true,
    hasCacheCreateTokens: true,
    hasToolCalls: true,
    hasToolResultEvents: true,
    hasSessionRelationships: true,
    hasRawContent: true,
  };
}

function fidelityWith(
  cls: Fidelity['class'],
  granularity: Fidelity['granularity'],
  overrides: Partial<Coverage> = {},
): Fidelity {
  return {
    class: cls,
    granularity,
    coverage: { ...fullCoverage(), ...overrides },
  };
}

function makeTurn(
  overrides: Partial<EnrichedTurn> & {
    sessionId: string;
    messageId: string;
    turnIndex: number;
    source: SourceKind;
  },
): EnrichedTurn {
  return {
    v: 1,
    model: 'claude-sonnet-4-6',
    ts: '2026-04-20T00:00:00.000Z',
    usage: {
      input: 100,
      output: 50,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    enrichment: {},
    ...overrides,
  };
}

function makeToolResultEvent(
  overrides: Partial<ToolResultEventRecord> & {
    sessionId: string;
    toolUseId: string;
    eventIndex: number;
    status: ToolResultEventRecord['status'];
  },
): ToolResultEventRecord {
  return {
    v: 1,
    source: 'claude-code',
    eventSource: 'tool_result',
    ...overrides,
  };
}

function args(flags: Record<string, string | true> = {}): ParsedArgs {
  return { flags, tags: {}, positional: [], passthrough: [] };
}

async function captureStdio<T>(
  fn: () => Promise<T>,
): Promise<{ result: T; stdout: string; stderr: string }> {
  let stdout = '';
  let stderr = '';
  const origOut = process.stdout.write.bind(process.stdout);
  const origErr = process.stderr.write.bind(process.stderr);
  process.stdout.write = ((c: string | Uint8Array) => {
    stdout += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((c: string | Uint8Array) => {
    stderr += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stderr.write;
  try {
    const result = await fn();
    return { result, stdout, stderr };
  } finally {
    process.stdout.write = origOut;
    process.stderr.write = origErr;
  }
}

const EMPTY_DEPS: HotspotsAttributionDeps = {
  loadContentBySession: async () => new Map(),
  loadUserTurnsBySession: async () => new Map(),
};

describe('turnPassesCoverage (#100)', () => {
  it('passes turns with no fidelity field (legacy ledger writers)', () => {
    const t = makeTurn({ sessionId: 's', messageId: 'm', turnIndex: 0, source: 'claude-code' });
    assert.equal(turnPassesCoverage(t, ['hasToolCalls', 'hasToolResultEvents']), true);
  });

  it('fails a turn that is missing any required coverage flag', () => {
    const t = makeTurn({
      sessionId: 's',
      messageId: 'm',
      turnIndex: 0,
      source: 'codex',
      fidelity: fidelityWith('partial', 'per-turn', { hasToolResultEvents: false }),
    });
    assert.equal(turnPassesCoverage(t, ['hasToolCalls', 'hasToolResultEvents']), false);
  });

  it('passes a turn that has every required coverage flag', () => {
    const t = makeTurn({
      sessionId: 's',
      messageId: 'm',
      turnIndex: 0,
      source: 'codex',
      fidelity: fidelityWith('full', 'per-turn'),
    });
    assert.equal(turnPassesCoverage(t, ['hasToolCalls', 'hasToolResultEvents']), true);
  });
});

describe('describeExcluded / source clauses (#100)', () => {
  it('groups excluded turns by source and tracks granularity + missing flags', () => {
    const excluded = [
      makeTurn({
        sessionId: 's1',
        messageId: 'm1',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('partial', 'per-turn', { hasToolResultEvents: false }),
      }),
      makeTurn({
        sessionId: 's1',
        messageId: 'm2',
        turnIndex: 1,
        source: 'codex',
        fidelity: fidelityWith('partial', 'per-turn', { hasToolResultEvents: false }),
      }),
      makeTurn({
        sessionId: 's2',
        messageId: 'm3',
        turnIndex: 0,
        source: 'opencode',
        fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
          hasToolCalls: false,
          hasToolResultEvents: false,
        }),
      }),
    ];
    const breakdown = describeExcluded(excluded, ATTRIBUTION_REQUIRED);
    assert.equal(breakdown.sources.size, 2);
    const codex = breakdown.sources.get('codex')!;
    assert.equal(codex.count, 2);
    assert.deepEqual([...codex.granularities].sort(), ['per-turn']);
    assert.deepEqual([...codex.missing].sort(), ['hasToolResultEvents']);
    const opencode = breakdown.sources.get('opencode')!;
    assert.equal(opencode.count, 1);
    assert.deepEqual([...opencode.granularities].sort(), ['per-session-aggregate']);
    assert.deepEqual(
      [...opencode.missing].sort(),
      ['hasToolCalls', 'hasToolResultEvents'],
    );

    const clause = renderSourcesClause(breakdown);
    assert.match(clause, /codex \(per-turn, missing tool-result events\)/);
    assert.match(
      clause,
      /opencode \(per-session-aggregate, missing tool-call records, tool-result events\)/,
    );
  });

  it('formatCoverageNotice renders an "analyzed N of M" line that names the gap and source', () => {
    const excluded = [
      makeTurn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('partial', 'per-turn', { hasToolResultEvents: false }),
      }),
      makeTurn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'codex',
        fidelity: fidelityWith('partial', 'per-turn', { hasToolResultEvents: false }),
      }),
    ];
    const breakdown = describeExcluded(excluded, ATTRIBUTION_REQUIRED);
    const notice = formatCoverageNotice(8, 10, breakdown);
    assert.match(notice, /^analyzed 8 of 10 turns; 2 excluded for /);
    assert.match(notice, /missing tool-result events/);
    assert.match(notice, /\(codex\)/);
  });

  it('fmtCoverageKey expands every key without falling through to a raw flag name', () => {
    const keys: Array<keyof Coverage> = [
      'hasInputTokens',
      'hasOutputTokens',
      'hasReasoningTokens',
      'hasCacheReadTokens',
      'hasCacheCreateTokens',
      'hasToolCalls',
      'hasToolResultEvents',
      'hasSessionRelationships',
      'hasRawContent',
    ];
    for (const k of keys) {
      const text = fmtCoverageKey(k);
      assert.ok(text && !text.startsWith('has'), `${k} -> ${text}`);
    }
  });
});

describe('PATTERN_REQUIRED prerequisites (#100)', () => {
  it('matches the spec: retries/failures can fall back to tool-call errors; reverts needs raw content', () => {
    assert.deepEqual([...PATTERN_REQUIRED.retries].sort(), ['hasToolCalls']);
    assert.deepEqual([...PATTERN_REQUIRED.failures].sort(), ['hasToolCalls']);
    assert.deepEqual([...PATTERN_REQUIRED.cancellations].sort(), ['hasToolCalls']);
    assert.deepEqual([...PATTERN_REQUIRED.reverts].sort(), [
      'hasRawContent',
      'hasToolCalls',
    ]);
  });
});

describe('resolvePatternSelection', () => {
  it('parses a comma-separated list of detector names', () => {
    const set = resolvePatternSelection('retries,failures');
    assert.equal(set.size, 2);
    assert.ok(set.has('retries'));
    assert.ok(set.has('failures'));
  });

  it('returns all detectors when the flag is bare (true)', () => {
    const set = resolvePatternSelection(true);
    // 8 inherited + cancellations (#113) + ghost-surface (#166) +
    // tool-output-bloat (#168) = 11.
    assert.equal(set.size, 11);
    assert.ok(set.has('cancellations'));
    assert.ok(set.has('ghost-surface'));
    assert.ok(set.has('tool-output-bloat'));
  });
});

describe('runHotspotsAttribution — fidelity refusal (#100)', () => {
  it('refuses with exit 2, names the missing prerequisite + source kind, when every turn is aggregate-only', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [];
    for (let i = 0; i < 142; i++) {
      turns.push(
        makeTurn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          source: 'codex',
          fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
            hasToolCalls: false,
            hasToolResultEvents: false,
          }),
        }),
      );
    }
    const { result, stdout, stderr } = await captureStdio(() =>
      runHotspotsAttribution(args(), turns, pricing, EMPTY_DEPS),
    );
    assert.equal(result, 2);
    assert.equal(stdout, '');
    assert.match(stderr, /burn hotspots: 142\/142 turns lack tool-call\/tool-result coverage/);
    assert.match(stderr, /codex/);
    assert.match(stderr, /per-session-aggregate/);
    assert.match(stderr, /missing tool-call records, tool-result events/);
    assert.match(stderr, /No hotspots analysis was performed/);
  });

  it('JSON-mode refusal still writes a fidelity block with refused: true and exits 2', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
          hasToolCalls: false,
          hasToolResultEvents: false,
        }),
      }),
    ];
    const { result, stdout, stderr } = await captureStdio(() =>
      runHotspotsAttribution(args({ json: true }), turns, pricing, EMPTY_DEPS),
    );
    assert.equal(result, 2);
    assert.match(stderr, /No hotspots analysis was performed/);
    const payload = JSON.parse(stdout);
    assert.equal(payload.fidelity.refused, true);
    assert.equal(payload.fidelity.analyzed, 0);
    assert.equal(payload.fidelity.excluded, 1);
    assert.ok(payload.fidelity.summary, 'summary present');
    assert.equal(payload.fidelity.summary.total, 1);
    assert.equal(payload.fidelity.summary.byClass['aggregate-only'], 1);
    assert.equal(payload.turnsAnalyzed, 0);
    assert.match(payload.refusalReason, /No hotspots analysis was performed/);
  });

  it('does not refuse on a fully empty input (no turns at all)', async () => {
    const pricing = await loadBuiltinPricing();
    const { result, stderr } = await captureStdio(() =>
      runHotspotsAttribution(args(), [], pricing, EMPTY_DEPS),
    );
    assert.equal(result, 0, 'empty slice is not a refusal');
    assert.equal(stderr, '');
  });
});

describe('runHotspotsAttribution — partial exclusion (#100)', () => {
  it('analyzes only qualifying turns and prints "analyzed N of M" with the exclusion reason', async () => {
    const pricing = await loadBuiltinPricing();
    const goodFidelity = fidelityWith('full', 'per-turn');
    const badFidelity = fidelityWith('partial', 'per-turn', { hasToolResultEvents: false });
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 'good',
        messageId: 'g1',
        turnIndex: 0,
        source: 'claude-code',
        fidelity: goodFidelity,
      }),
      makeTurn({
        sessionId: 'good',
        messageId: 'g2',
        turnIndex: 1,
        source: 'claude-code',
        fidelity: goodFidelity,
      }),
      makeTurn({
        sessionId: 'bad',
        messageId: 'b1',
        turnIndex: 0,
        source: 'codex',
        fidelity: badFidelity,
      }),
    ];
    const { result, stdout, stderr } = await captureStdio(() =>
      runHotspotsAttribution(args(), turns, pricing, EMPTY_DEPS),
    );
    assert.equal(result, 0);
    assert.equal(stderr, '');
    assert.match(stdout, /turns analyzed: 2/);
    assert.match(stdout, /analyzed 2 of 3 turns; 1 excluded for/);
    assert.match(stdout, /missing tool-result events/);
    assert.match(stdout, /\(codex\)/);
  });

  it('omits the coverage notice when nothing is excluded', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm',
        turnIndex: 0,
        source: 'claude-code',
        fidelity: fidelityWith('full', 'per-turn'),
      }),
    ];
    const { result, stdout } = await captureStdio(() =>
      runHotspotsAttribution(args(), turns, pricing, EMPTY_DEPS),
    );
    assert.equal(result, 0);
    assert.doesNotMatch(stdout, /analyzed \d+ of \d+ turns; \d+ excluded/);
  });

  it('JSON mode includes a fidelity block with analyzed, excluded, summary, refused: false', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 'good',
        messageId: 'g1',
        turnIndex: 0,
        source: 'claude-code',
        fidelity: fidelityWith('full', 'per-turn'),
      }),
      makeTurn({
        sessionId: 'bad',
        messageId: 'b1',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('partial', 'per-turn', { hasToolResultEvents: false }),
      }),
    ];
    const { result, stdout } = await captureStdio(() =>
      runHotspotsAttribution(args({ json: true }), turns, pricing, EMPTY_DEPS),
    );
    assert.equal(result, 0);
    const payload = JSON.parse(stdout);
    assert.equal(payload.fidelity.refused, false);
    assert.equal(payload.fidelity.analyzed, 1);
    assert.equal(payload.fidelity.excluded, 1);
    assert.equal(payload.fidelity.summary.total, 2);
    assert.equal(payload.fidelity.summary.byClass.full, 1);
    assert.equal(payload.fidelity.summary.byClass.partial, 1);
    assert.equal(payload.turnsAnalyzed, 1);
  });
});

describe('runHotspotsAttribution — bulk session loaders', () => {
  it('invokes loadUserTurnsBySession and loadContentBySession once with the eligible session ids, not once per session', async () => {
    const pricing = await loadBuiltinPricing();
    const goodFidelity = fidelityWith('full', 'per-turn');
    const badFidelity = fidelityWith('partial', 'per-turn', { hasToolResultEvents: false });
    const turns: EnrichedTurn[] = [
      makeTurn({ sessionId: 's-1', messageId: 'a', turnIndex: 0, source: 'claude-code', fidelity: goodFidelity }),
      makeTurn({ sessionId: 's-2', messageId: 'b', turnIndex: 0, source: 'claude-code', fidelity: goodFidelity }),
      makeTurn({ sessionId: 's-3', messageId: 'c', turnIndex: 0, source: 'claude-code', fidelity: goodFidelity }),
      // Excluded — its sessionId must not appear in the eligible-only loader call.
      makeTurn({ sessionId: 's-skip', messageId: 'x', turnIndex: 0, source: 'codex', fidelity: badFidelity }),
    ];

    const userCalls: Array<Set<string>> = [];
    const contentCalls: Array<Set<string>> = [];
    const deps: HotspotsAttributionDeps = {
      loadUserTurnsBySession: async (ids) => {
        userCalls.push(new Set(ids));
        return new Map();
      },
      loadContentBySession: async (ids) => {
        contentCalls.push(new Set(ids));
        return new Map();
      },
    };

    const { result } = await captureStdio(() =>
      runHotspotsAttribution(args(), turns, pricing, deps),
    );
    assert.equal(result, 0);
    assert.equal(userCalls.length, 1, 'user-turn loader must be called exactly once');
    assert.equal(contentCalls.length, 1, 'content loader must be called exactly once');
    assert.deepEqual([...userCalls[0]!].sort(), ['s-1', 's-2', 's-3']);
    assert.deepEqual([...contentCalls[0]!].sort(), ['s-1', 's-2', 's-3']);
  });
});

describe('runPatternsMode — fidelity refusal (#100)', () => {
  it('refuses with exit 2 when every turn is below every selected detector\'s prereq', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
          hasToolCalls: false,
          hasToolResultEvents: false,
          hasRawContent: false,
        }),
      }),
    ];
    const selected = new Set(['retries', 'failures'] as const);
    const { result, stdout, stderr } = await captureStdio(() =>
      runPatternsMode(args(), turns, pricing, [], selected),
    );
    assert.equal(result, 2);
    assert.equal(stdout, '');
    assert.match(stderr, /burn hotspots --patterns: no selected detectors can run/);
    assert.match(stderr, /retries: 1\/1 turns lack tool-call records/);
    assert.match(stderr, /failures: 1\/1 turns lack tool-call records/);
    assert.match(stderr, /codex/);
  });

  it('JSON-mode refusal includes a findings: [] field for schema parity (#56)', async () => {
    // Devin review on #175: a JSON consumer that processes the success
    // schema (which always carries `findings`) shouldn't have to special-
    // case refusal payloads. Mirror the pattern used by `retryLoops` etc.
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
          hasToolCalls: false,
          hasToolResultEvents: false,
        }),
      }),
    ];
    const selected = new Set(['retries'] as const);
    const { stdout } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected),
    );
    const payload = JSON.parse(stdout);
    assert.deepEqual(payload.findings, []);
  });

  it('JSON-mode refusal includes per-detector required prerequisites and refused=true', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
          hasToolCalls: false,
          hasToolResultEvents: false,
        }),
      }),
    ];
    const selected = new Set(['retries'] as const);
    const { stdout } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected),
    );
    const payload = JSON.parse(stdout);
    assert.equal(payload.fidelity.refused, true);
    assert.ok(Array.isArray(payload.fidelity.perDetector));
    const retries = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'retries',
    );
    assert.ok(retries, 'retries detector reported');
    assert.deepEqual(retries.required.sort(), ['hasToolCalls']);
    assert.equal(retries.refused, true);
    assert.equal(retries.analyzed, 0);
    assert.equal(retries.excluded, 1);
    assert.ok(Array.isArray(retries.excludedBySource));
    assert.equal(retries.excludedBySource[0].source, 'codex');
  });
});

describe('runPatternsMode — per-detector partial exclusion (#100)', () => {
  it('names the missing coverage flag per detector when a source is excluded', async () => {
    const pricing = await loadBuiltinPricing();
    // Three claude turns with full fidelity; two codex turns without
    // tool-call records. Selecting --patterns retries,failures should analyze
    // only the claude turns and emit a per-detector notice naming the missing
    // prereq.
    const turns: EnrichedTurn[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(
        makeTurn({
          sessionId: 'good',
          messageId: `g${i}`,
          turnIndex: i,
          source: 'claude-code',
          fidelity: fidelityWith('full', 'per-turn'),
        }),
      );
    }
    for (let i = 0; i < 2; i++) {
      turns.push(
        makeTurn({
          sessionId: 'bad',
          messageId: `b${i}`,
          turnIndex: i,
          source: 'codex',
          fidelity: fidelityWith('partial', 'per-turn', {
            hasToolCalls: false,
            hasToolResultEvents: false,
          }),
        }),
      );
    }
    const selected = new Set(['retries', 'failures'] as const);
    const { result, stdout, stderr } = await captureStdio(() =>
      runPatternsMode(args(), turns, pricing, [], selected),
    );
    assert.equal(result, 0);
    assert.equal(stderr, '');
    // Per-detector lines should mention the missing prereq + source.
    assert.match(stdout, /retries: analyzed 3 of 5 turns; 2 excluded \(needs tool-call records;/);
    assert.match(stdout, /failures: analyzed 3 of 5 turns; 2 excluded \(needs tool-call records;/);
    assert.match(stdout, /missing tool-call records/);
    assert.match(stdout, /\(codex\)/);
  });

  it('JSON mode reports per-detector required + excludedBySource shape', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 'good',
        messageId: 'g1',
        turnIndex: 0,
        source: 'claude-code',
        fidelity: fidelityWith('full', 'per-turn'),
      }),
      makeTurn({
        sessionId: 'bad',
        messageId: 'b1',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('partial', 'per-turn', {
          hasToolCalls: false,
          hasToolResultEvents: false,
        }),
      }),
    ];
    const selected = new Set(['retries', 'failures', 'reverts'] as const);
    const { stdout } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected),
    );
    const payload = JSON.parse(stdout);
    assert.equal(payload.fidelity.refused, false);
    const retries = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'retries',
    );
    const reverts = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'reverts',
    );
    assert.ok(retries && reverts);
    assert.deepEqual(retries.required.sort(), ['hasToolCalls']);
    assert.deepEqual(reverts.required.sort(), ['hasRawContent', 'hasToolCalls']);
    // The codex turn here lacks tool-call records, so it fails both retries
    // and reverts.
    assert.equal(retries.excluded, 1);
    assert.equal(retries.excludedBySource[0].source, 'codex');
    assert.deepEqual(
      retries.excludedBySource[0].missingCoverage.sort(),
      ['hasToolCalls'],
    );
  });

  it('compaction detector is independent of fidelity — runs against the full slice', async () => {
    const pricing = await loadBuiltinPricing();
    // Even though every turn lacks tool coverage, selecting only `compaction`
    // must not refuse — the compaction sidecar comes from the ledger directly.
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        source: 'codex',
        fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
          hasToolCalls: false,
          hasToolResultEvents: false,
        }),
      }),
    ];
    const selected = new Set(['compaction'] as const);
    const { result, stdout, stderr } = await captureStdio(() =>
      runPatternsMode(args(), turns, pricing, [], selected),
    );
    assert.equal(result, 0, 'compaction-only must not refuse on aggregate-only input');
    assert.equal(stderr, '');
    assert.doesNotMatch(stdout, /no selected detectors can run/);
  });

  it('compaction-only counts every turn as analyzed (top-level + JSON fidelity)', async () => {
    const pricing = await loadBuiltinPricing();
    // Regression for the case where --patterns compaction reported
    // turnsAnalyzed: 0 / fidelity.excluded: total because the analyzed-union
    // skipped the compaction slice. Compaction has no fidelity prereq, so
    // every turn is "analyzed" by it.
    const turns: EnrichedTurn[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(
        makeTurn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          source: 'codex',
          fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
            hasToolCalls: false,
            hasToolResultEvents: false,
          }),
        }),
      );
    }
    const selected = new Set(['compaction'] as const);
    const { stdout } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected),
    );
    const payload = JSON.parse(stdout);
    assert.equal(payload.turnsAnalyzed, 3);
    assert.equal(payload.fidelity.analyzed, 3);
    assert.equal(payload.fidelity.excluded, 0);
    assert.equal(payload.fidelity.refused, false);
    const compaction = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'compaction',
    );
    assert.ok(compaction);
    assert.equal(compaction.analyzed, 3);
    assert.equal(compaction.excluded, 0);
  });

  it('mixed compaction + retries union credits compaction-only turns', async () => {
    const pricing = await loadBuiltinPricing();
    // Two full-fidelity turns (analyzable by retries) + three aggregate-only
    // turns (only compaction can analyze). The union should be all 5 turns;
    // fidelity.excluded must be 0 because every turn was analyzed by at
    // least one detector.
    const turns: EnrichedTurn[] = [];
    for (let i = 0; i < 2; i++) {
      turns.push(
        makeTurn({
          sessionId: 'good',
          messageId: `g${i}`,
          turnIndex: i,
          source: 'claude-code',
          fidelity: fidelityWith('full', 'per-turn'),
        }),
      );
    }
    for (let i = 0; i < 3; i++) {
      turns.push(
        makeTurn({
          sessionId: 'bad',
          messageId: `b${i}`,
          turnIndex: i,
          source: 'codex',
          fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
            hasToolCalls: false,
            hasToolResultEvents: false,
          }),
        }),
      );
    }
    const selected = new Set(['retries', 'compaction'] as const);
    const { stdout } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected),
    );
    const payload = JSON.parse(stdout);
    assert.equal(payload.turnsAnalyzed, 5);
    assert.equal(payload.fidelity.analyzed, 5);
    assert.equal(payload.fidelity.excluded, 0);
    const retries = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'retries',
    );
    assert.equal(retries.analyzed, 2);
    assert.equal(retries.excluded, 3);
    const compaction = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'compaction',
    );
    assert.equal(compaction.analyzed, 5);
    assert.equal(compaction.excluded, 0);
  });

  it('mixed retries+compaction does NOT refuse when only retries lacks coverage', async () => {
    const pricing = await loadBuiltinPricing();
    // Every turn is aggregate-only — retries must refuse, but compaction
    // has no fidelity prereq and should still run. Refusing the whole
    // command in this case would silently drop the compaction signal.
    const turns: EnrichedTurn[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(
        makeTurn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          source: 'codex',
          fidelity: fidelityWith('aggregate-only', 'per-session-aggregate', {
            hasToolCalls: false,
            hasToolResultEvents: false,
          }),
        }),
      );
    }
    const selected = new Set(['retries', 'compaction'] as const);
    const { result, stdout, stderr } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected),
    );
    assert.equal(result, 0, 'must not refuse — compaction can still run');
    assert.equal(stderr, '');
    const payload = JSON.parse(stdout);
    assert.equal(payload.fidelity.refused, false);
    assert.equal(payload.turnsAnalyzed, 3);
    const retries = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'retries',
    );
    assert.equal(retries.refused, true);
    assert.equal(retries.analyzed, 0);
    const compaction = payload.fidelity.perDetector.find(
      (d: { kind: string }) => d.kind === 'compaction',
    );
    assert.equal(compaction.refused, false);
    assert.equal(compaction.analyzed, 3);
  });

  it('JSON mode annotates graph-backed retry findings with eventSource (#113)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        source: 'claude-code',
        toolCalls: [{ id: 'u0', name: 'Bash', argsHash: 'same' }],
      }),
      makeTurn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        source: 'claude-code',
        toolCalls: [{ id: 'u1', name: 'Bash', argsHash: 'same' }],
      }),
      makeTurn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 2,
        source: 'claude-code',
        toolCalls: [{ id: 'u2', name: 'Bash', argsHash: 'same' }],
      }),
    ];
    const toolResultEvents = [
      makeToolResultEvent({ sessionId: 's', toolUseId: 'u0', eventIndex: 0, status: 'errored' }),
      makeToolResultEvent({ sessionId: 's', toolUseId: 'u1', eventIndex: 1, status: 'errored' }),
      makeToolResultEvent({ sessionId: 's', toolUseId: 'u2', eventIndex: 2, status: 'errored' }),
    ];
    const selected = new Set(['retries'] as const);
    const { stdout } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected, {
        toolResultEvents,
      }),
    );

    const payload = JSON.parse(stdout);
    assert.equal(payload.retryLoops[0].eventSource, 'tool_result');
    assert.equal(payload.findings[0].eventSource, 'tool_result');
  });

  it('JSON mode separates cancelled graph events from retry/failure output (#113)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      makeTurn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        source: 'claude-code',
        toolCalls: [{ id: 'u0', name: 'Bash', argsHash: 'same', isError: true }],
      }),
      makeTurn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        source: 'claude-code',
        toolCalls: [{ id: 'u1', name: 'Bash', argsHash: 'same', isError: true }],
      }),
      makeTurn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 2,
        source: 'claude-code',
        toolCalls: [{ id: 'u2', name: 'Bash', argsHash: 'same', isError: true }],
      }),
    ];
    const toolResultEvents = [
      makeToolResultEvent({ sessionId: 's', toolUseId: 'u0', eventIndex: 0, status: 'cancelled' }),
      makeToolResultEvent({ sessionId: 's', toolUseId: 'u1', eventIndex: 1, status: 'cancelled' }),
      makeToolResultEvent({ sessionId: 's', toolUseId: 'u2', eventIndex: 2, status: 'cancelled' }),
    ];
    const selected = new Set(['retries', 'failures'] as const);
    const { stdout } = await captureStdio(() =>
      runPatternsMode(args({ json: true }), turns, pricing, [], selected, {
        toolResultEvents,
      }),
    );

    const payload = JSON.parse(stdout);
    assert.deepEqual(payload.retryLoops, []);
    assert.deepEqual(payload.failureRuns, []);
    assert.equal(payload.cancelledRuns.length, 1);
    assert.equal(payload.cancelledRuns[0].eventSource, 'tool_result');
    assert.equal(payload.findings[0].kind, 'cancellation-run');
  });
});
