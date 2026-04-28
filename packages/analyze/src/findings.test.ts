import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type {
  CompactionLoss,
  EditHeavySession,
  EditRevertCycle,
  FailureRun,
  PatternsResult,
  RetryLoop,
  SessionPatternSummary,
  SkillPruningProtection,
  SkillRecallDup,
  SystemPromptTax,
} from './patterns.js';

import {
  compactionLossToFinding,
  editHeavyToFinding,
  editRevertToFinding,
  failureRunToFinding,
  findingsFromPatterns,
  retryLoopToFinding,
  skillPruningProtectionToFinding,
  skillRecallDupToFinding,
  sortFindings,
  systemPromptTaxToFinding,
  type WasteFinding,
} from './findings.js';

const SESSION = '11111111-2222-3333-4444-555555555555';

const baseRetryLoop: RetryLoop = {
  sessionId: SESSION,
  tool: 'Bash',
  target: 'pnpm test',
  argsHash: 'abc123',
  attempts: 4,
  startTurnIndex: 0,
  endTurnIndex: 3,
  cost: 0.6,
};

describe('findings — retry loop adapter', () => {
  it('maps RetryLoop to a WasteFinding with kind "retry-loop"', () => {
    const f = retryLoopToFinding(baseRetryLoop);
    assert.equal(f.kind, 'retry-loop');
    assert.equal(f.sessionId, SESSION);
    assert.equal(f.severity, 'high');
    assert.match(f.title, /Bash pnpm test failed 4× in a row/);
    assert.match(f.detail, /Turns 0-3/);
    assert.equal(f.estimatedSavings.usdPerSession, 0.6);
    assert.equal(f.actions.length, 1);
    assert.equal(f.actions[0]!.type, 'command');
    assert.match((f.actions[0] as { text: string }).text, /burn diagnose/);
  });

  it('downgrades severity below thresholds', () => {
    assert.equal(retryLoopToFinding({ ...baseRetryLoop, cost: 0.0001 }).severity, 'info');
    assert.equal(retryLoopToFinding({ ...baseRetryLoop, cost: 0.1 }).severity, 'warn');
    assert.equal(retryLoopToFinding({ ...baseRetryLoop, cost: 1.0 }).severity, 'high');
  });
});

describe('findings — failure run adapter', () => {
  it('maps FailureRun to a WasteFinding with the involved tool list', () => {
    const fr: FailureRun = {
      sessionId: SESSION,
      length: 3,
      startTurnIndex: 5,
      endTurnIndex: 7,
      toolsInvolved: ['Bash', 'Read', 'Edit'],
      cost: 0.08,
    };
    const f = failureRunToFinding(fr);
    assert.equal(f.kind, 'failure-run');
    assert.equal(f.severity, 'warn');
    assert.match(f.detail, /Bash, Read, Edit/);
  });
});

describe('findings — compaction-loss adapter', () => {
  it('exposes tokensBeforeCompact via tokensPerSession', () => {
    const c: CompactionLoss = {
      sessionId: SESSION,
      ts: '2026-04-20T00:00:00.000Z',
      precedingMessageId: 'msg-1',
      tokensBeforeCompact: 9000,
      cacheLostCost: 0.04,
    };
    const f = compactionLossToFinding(c);
    assert.equal(f.kind, 'compaction-loss');
    assert.equal(f.estimatedSavings.tokensPerSession, 9000);
    assert.equal(f.estimatedSavings.usdPerSession, 0.04);
    assert.equal(f.severity, 'info');
  });

  it('omits tokensPerSession when tokensBeforeCompact is zero', () => {
    const c: CompactionLoss = {
      sessionId: SESSION,
      ts: '2026-04-20T00:00:00.000Z',
      precedingMessageId: undefined,
      tokensBeforeCompact: 0,
      cacheLostCost: 0,
    };
    const f = compactionLossToFinding(c);
    assert.equal(f.estimatedSavings.tokensPerSession, undefined);
  });
});

describe('findings — edit-revert adapter', () => {
  it('maps EditRevertCycle to a WasteFinding', () => {
    const e: EditRevertCycle = {
      sessionId: SESSION,
      filePath: 'src/foo.ts',
      firstEditTurnIndex: 2,
      revertTurnIndex: 8,
      spanTurns: 6,
      cost: 0.72,
    };
    const f = editRevertToFinding(e);
    assert.equal(f.kind, 'edit-revert');
    assert.equal(f.severity, 'high');
    assert.match(f.detail, /6 turns later/);
    assert.match(f.title, /src\/foo\.ts/);
  });
});

describe('findings — edit-heavy adapter', () => {
  it('caps severity at warn even for high-cost sessions', () => {
    // Edit-heavy is a fuzzy signal whose cost overlaps with retry/revert
    // findings; severity is capped to avoid double-promoting the same
    // dollars to "high" via two findings. Documented in findings.ts.
    const s: EditHeavySession = {
      source: 'claude-code',
      sessionId: SESSION,
      readCount: 1,
      editCount: 12,
      ratio: 12,
      likelyRetries: 3,
      cost: 5,
    };
    const f = editHeavyToFinding(s);
    assert.equal(f.kind, 'edit-heavy');
    assert.equal(f.severity, 'warn');
    assert.match(f.title, /12 edits \/ 1 reads/);
  });

  it('renders ratio as ∞ when reads is zero', () => {
    const s: EditHeavySession = {
      source: 'codex',
      sessionId: SESSION,
      readCount: 0,
      editCount: 6,
      ratio: Number.POSITIVE_INFINITY,
      likelyRetries: 0,
      cost: 0.01,
    };
    const f = editHeavyToFinding(s);
    assert.match(f.title, /ratio ∞/);
  });
});

describe('findings — opencode adapters', () => {
  it('maps SkillRecallDup', () => {
    const d: SkillRecallDup = {
      sessionId: SESSION,
      skillName: 'init',
      callCount: 3,
      firstTurnIndex: 1,
      lastTurnIndex: 11,
      cost: 0.2,
    };
    const f = skillRecallDupToFinding(d);
    assert.equal(f.kind, 'skill-recall-dup');
    assert.match(f.title, /init.*3×/);
  });

  it('maps SkillPruningProtection', () => {
    const p: SkillPruningProtection = {
      sessionId: SESSION,
      skillName: 'init',
      invokedTurnIndex: 0,
      ridingTurns: 7,
      lastCachedTurnIndex: 7,
      cost: 0.55,
    };
    const f = skillPruningProtectionToFinding(p);
    assert.equal(f.kind, 'skill-pruning-protection');
    assert.equal(f.severity, 'high');
  });

  it('maps SystemPromptTax with ridingTokens estimate', () => {
    const t: SystemPromptTax = {
      sessionId: SESSION,
      firstTurnCacheCreate: 4500,
      firstUserMessageTokens: 500,
      estimatedSystemPromptTokens: 4000,
      ridingTurns: 6,
      totalCost: 0.07,
    };
    const f = systemPromptTaxToFinding(t);
    assert.equal(f.kind, 'system-prompt-tax');
    assert.equal(f.estimatedSavings.tokensPerSession, 4000 * 6);
    assert.equal(f.estimatedSavings.usdPerSession, 0.07);
  });
});

describe('findings — sortFindings', () => {
  it('sorts high before warn before info, then by usdPerSession descending', () => {
    const findings: WasteFinding[] = [
      {
        kind: 'a',
        severity: 'info',
        sessionId: 's1',
        title: 'a',
        detail: '',
        estimatedSavings: { usdPerSession: 0.001 },
        actions: [],
      },
      {
        kind: 'b',
        severity: 'high',
        sessionId: 's2',
        title: 'b',
        detail: '',
        estimatedSavings: { usdPerSession: 0.6 },
        actions: [],
      },
      {
        kind: 'c',
        severity: 'high',
        sessionId: 's3',
        title: 'c',
        detail: '',
        estimatedSavings: { usdPerSession: 1.2 },
        actions: [],
      },
      {
        kind: 'd',
        severity: 'warn',
        sessionId: 's4',
        title: 'd',
        detail: '',
        estimatedSavings: { usdPerSession: 0.3 },
        actions: [],
      },
    ];
    const sorted = sortFindings(findings);
    assert.deepEqual(
      sorted.map((f) => f.kind),
      ['c', 'b', 'd', 'a'],
    );
  });
});

describe('findings — findingsFromPatterns rolls up the full PatternsResult', () => {
  it('emits one finding per detector entry across kinds', () => {
    const summary: SessionPatternSummary = {
      sessionId: SESSION,
      retryLoopCount: 1,
      failureRunCount: 1,
      consecutiveFailureMax: 3,
      compactionCount: 1,
      editRevertCount: 1,
      skillRecallDupCount: 0,
      skillPruningProtectionCount: 0,
      systemPromptTaxCount: 0,
      editHeavyCount: 0,
      totalRetries: 4,
      totalPatternCost: 1.5,
    };
    const result: PatternsResult = {
      retryLoops: [baseRetryLoop],
      failureRuns: [
        {
          sessionId: SESSION,
          length: 3,
          startTurnIndex: 0,
          endTurnIndex: 2,
          toolsInvolved: ['Bash', 'Edit'],
          cost: 0.05,
        },
      ],
      compactions: [
        {
          sessionId: SESSION,
          ts: '2026-04-20T00:00:00.000Z',
          precedingMessageId: 'm',
          tokensBeforeCompact: 9000,
          cacheLostCost: 0.04,
        },
      ],
      editReverts: [
        {
          sessionId: SESSION,
          filePath: 'src/foo.ts',
          firstEditTurnIndex: 1,
          revertTurnIndex: 4,
          spanTurns: 3,
          cost: 0.2,
        },
      ],
      editHeavySessions: [],
      skillRecallDups: [],
      skillPruningProtection: [],
      systemPromptTaxes: [],
      sessionSummaries: [summary],
    };
    const findings = findingsFromPatterns(result);
    assert.equal(findings.length, 4);
    const kinds = new Set(findings.map((f) => f.kind));
    assert.ok(kinds.has('retry-loop'));
    assert.ok(kinds.has('failure-run'));
    assert.ok(kinds.has('compaction-loss'));
    assert.ok(kinds.has('edit-revert'));
    // Must be sorted: retry-loop ($0.6, high) ranks first.
    assert.equal(findings[0]!.kind, 'retry-loop');
  });
});
