import { strict as assert } from 'node:assert';
import { mkdtemp, writeFile, mkdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, it, after, beforeEach } from 'node:test';

import { parseClaudeSession, parseCodexSession } from '@relayburn/reader';
import type {
  ToolCall,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES_ROOT = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures');

import { loadBuiltinPricing } from './pricing.js';
import {
  BASH_MAX_OUTPUT_ENV_KEY,
  DEFAULT_BLOAT_TOKEN_THRESHOLD,
  detectObservedBloat,
  detectStaticConfigBloat,
  detectToolOutputBloat,
  loadClaudeSettings,
  toolOutputBloatToFinding,
  userClaudeSettingsPath,
  type LoadedClaudeSettings,
} from './tool-output-bloat.js';

function tc(id: string, name: string, opts: Partial<ToolCall> = {}): ToolCall {
  return { id, name, argsHash: 'hash', ...opts };
}

function turn(o: Partial<TurnRecord> & { sessionId: string; messageId: string; turnIndex: number }): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    model: 'claude-sonnet-4-6',
    ts: '2026-04-20T00:00:00.000Z',
    usage: { input: 10, output: 5, reasoning: 0, cacheRead: 100, cacheCreate5m: 50, cacheCreate1h: 0 },
    toolCalls: [],
    ...o,
  };
}

function evt(
  o: Partial<ToolResultEventRecord> & { sessionId: string; toolUseId: string; eventIndex: number },
): ToolResultEventRecord {
  return {
    v: 1,
    source: 'claude-code',
    eventSource: 'tool_result',
    status: 'completed',
    ...o,
  };
}

function userTurn(
  o: Partial<UserTurnRecord> & { sessionId: string; userUuid: string },
): UserTurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    ts: '2026-04-20T00:00:00.500Z',
    blocks: [],
    ...o,
  };
}

// ---------------------------------------------------------------------------
// Signal A — static config check
// ---------------------------------------------------------------------------

describe('detectStaticConfigBloat — Signal A', () => {
  it('flags BASH_MAX_OUTPUT_LENGTH whose token-equivalent exceeds the threshold', () => {
    const settings: LoadedClaudeSettings[] = [
      {
        path: '/home/u/.claude/settings.json',
        // 80,000 chars ≈ 20,000 tokens — above the 15k-token threshold.
        settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '80000' } },
      },
    ];
    const out = detectStaticConfigBloat({ settings });
    assert.equal(out.length, 1);
    const flag = out[0]!;
    assert.equal(flag.kind, 'static-config');
    assert.equal(flag.source, 'claude-code');
    assert.equal(flag.toolName, 'Bash');
    // `configuredLimit` keeps the raw env value (chars) so the user knows
    // exactly what they typed; `evidencedMaxOutput` is in tokens so it lines
    // up with Signal B.
    assert.equal(flag.configuredLimit, 80000);
    assert.equal(flag.evidencedMaxOutput, 20000);
    assert.equal(flag.occurrenceCount, 1);
    assert.equal(flag.cost, 0);
    assert.deepEqual(flag.evidence, ['/home/u/.claude/settings.json']);
  });

  it('does NOT flag at the 15k-token char-equivalent (60000 chars)', () => {
    const settings: LoadedClaudeSettings[] = [
      // 60,000 chars = 15,000 tokens — exactly at the threshold, not above.
      { path: '/u/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '60000' } } },
    ];
    assert.equal(detectStaticConfigBloat({ settings }).length, 0);
  });

  it('does NOT flag a permissive char value whose token-equivalent is under threshold', () => {
    // 50,000 chars ≈ 12,500 tokens — below threshold despite looking large in
    // chars. This is the regression that motivated the unit-conversion fix:
    // pre-fix, the detector would have flagged this as "above 15000".
    const settings: LoadedClaudeSettings[] = [
      { path: '/u/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '50000' } } },
    ];
    assert.equal(detectStaticConfigBloat({ settings }).length, 0);
  });

  it('returns nothing when env block is absent', () => {
    const settings: LoadedClaudeSettings[] = [
      { path: '/u/.claude/settings.json', settings: {} },
    ];
    assert.equal(detectStaticConfigBloat({ settings }).length, 0);
  });

  it('project settings override user settings (last-wins precedence)', () => {
    const settings: LoadedClaudeSettings[] = [
      // User: 80,000 chars ≈ 20,000 tokens (would fire).
      { path: '/u/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '80000' } } },
      // Project override at threshold (60,000 chars = 15,000 tokens) — should
      // NOT fire even though user is bad.
      { path: '/cwd/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '60000' } } },
    ];
    assert.equal(detectStaticConfigBloat({ settings }).length, 0);
  });

  it('reports the project settings path when project flips a permissive user value', () => {
    const settings: LoadedClaudeSettings[] = [
      // User: 15,000 chars ≈ 3,750 tokens (well under).
      { path: '/u/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '15000' } } },
      // Project: 99,999 chars ≈ 25,000 tokens (over).
      { path: '/cwd/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '99999' } } },
    ];
    const out = detectStaticConfigBloat({ settings });
    assert.equal(out.length, 1);
    assert.deepEqual(out[0]!.evidence, ['/cwd/.claude/settings.json']);
    assert.equal(out[0]!.configuredLimit, 99999);
  });

  it('honors a custom threshold', () => {
    // 5,000 chars ≈ 1,250 tokens. Threshold 1000 (tokens) → flags;
    // threshold 10000 (tokens) → doesn't flag.
    const settings: LoadedClaudeSettings[] = [
      { path: '/u/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '5000' } } },
    ];
    assert.equal(detectStaticConfigBloat({ settings, threshold: 1000 }).length, 1);
    assert.equal(detectStaticConfigBloat({ settings, threshold: 10000 }).length, 0);
  });
});

// ---------------------------------------------------------------------------
// Filesystem loader for Signal A
// ---------------------------------------------------------------------------

describe('loadClaudeSettings — filesystem loader', () => {
  let tmp: string;
  const originalHome = process.env['HOME'];

  beforeEach(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'tool-output-bloat-'));
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
  });

  it('returns undefined when the file does not exist', async () => {
    const out = await loadClaudeSettings(path.join(tmp, 'nope.json'));
    assert.equal(out, undefined);
  });

  it('returns undefined for malformed JSON (does not throw)', async () => {
    const file = path.join(tmp, 'bad.json');
    await writeFile(file, '{not json', 'utf8');
    const out = await loadClaudeSettings(file);
    assert.equal(out, undefined);
  });

  it('reads valid settings.json and exposes the env block', async () => {
    const file = path.join(tmp, 'settings.json');
    await writeFile(file, JSON.stringify({ env: { [BASH_MAX_OUTPUT_ENV_KEY]: '80000' } }), 'utf8');
    const out = await loadClaudeSettings(file);
    assert.ok(out);
    assert.equal(out!.path, file);
    assert.equal(out!.settings.env?.[BASH_MAX_OUTPUT_ENV_KEY], '80000');
  });

  it('userClaudeSettingsPath() honors HOME', async () => {
    process.env['HOME'] = tmp;
    const expected = path.join(tmp, '.claude', 'settings.json');
    assert.equal(userClaudeSettingsPath(), expected);
  });

  it('end-to-end: load + detect from an isolated HOME', async () => {
    process.env['HOME'] = tmp;
    const claudeDir = path.join(tmp, '.claude');
    await mkdir(claudeDir, { recursive: true });
    const file = path.join(claudeDir, 'settings.json');
    // 80,000 chars ≈ 20,000 tokens — above the 15k-token threshold.
    await writeFile(file, JSON.stringify({ env: { [BASH_MAX_OUTPUT_ENV_KEY]: '80000' } }), 'utf8');
    const loaded = await loadClaudeSettings(userClaudeSettingsPath());
    assert.ok(loaded);
    const result = detectStaticConfigBloat({ settings: [loaded!] });
    assert.equal(result.length, 1);
    assert.equal(result[0]!.configuredLimit, 80000);
  });
});

// ---------------------------------------------------------------------------
// Signal B — observed bloat across sessions
// ---------------------------------------------------------------------------

describe('detectObservedBloat — Signal B', () => {
  it('flags Claude Bash tool_result events above the 15k-token threshold', async () => {
    const pricing = await loadBuiltinPricing();
    // 20,000 tokens — well above the 15k threshold.
    const events = [
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_a', byteLen: 80_000, approxTokens: 20_000 }],
      }),
    ];
    const turns = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
    ];
    const out = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing });
    assert.equal(out.length, 1);
    const b = out[0]!;
    assert.equal(b.kind, 'observed-bloat');
    assert.equal(b.source, 'claude-code');
    assert.equal(b.toolName, 'Bash');
    assert.equal(b.occurrenceCount, 1);
    assert.equal(b.evidencedMaxOutput, 20_000);
    assert.deepEqual(b.evidence, ['s1']);
    assert.ok(b.cost > 0, 'cost should be priced via the model rate');
  });

  it('does NOT flag below the threshold', async () => {
    const pricing = await loadBuiltinPricing();
    // 10,000 tokens — under threshold.
    const events = [
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_a', byteLen: 40_000, approxTokens: 10_000 }],
      }),
    ];
    const turns = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
    ];
    const out = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing });
    assert.equal(out.length, 0);
  });

  it('aggregates multiple oversized events into one (source, toolName) bucket', async () => {
    const pricing = await loadBuiltinPricing();
    const events = [
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
      evt({ sessionId: 's2', toolUseId: 'tu_b', eventIndex: 0, messageId: 'm2' }),
      evt({ sessionId: 's3', toolUseId: 'tu_c', eventIndex: 0, messageId: 'm3' }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_a', byteLen: 80_000, approxTokens: 20_000 }],
      }),
      userTurn({
        sessionId: 's2',
        userUuid: 'u2',
        precedingMessageId: 'm2',
        followingMessageId: 'm3',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_b', byteLen: 100_000, approxTokens: 25_000 }],
      }),
      userTurn({
        sessionId: 's3',
        userUuid: 'u3',
        precedingMessageId: 'm3',
        followingMessageId: 'm4',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_c', byteLen: 120_000, approxTokens: 30_000 }],
      }),
    ];
    const turns = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
      turn({ sessionId: 's2', messageId: 'm2', turnIndex: 0, toolCalls: [tc('tu_b', 'Bash')] }),
      turn({ sessionId: 's3', messageId: 'm3', turnIndex: 0, toolCalls: [tc('tu_c', 'Bash')] }),
    ];
    const out = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing });
    assert.equal(out.length, 1);
    const b = out[0]!;
    assert.equal(b.occurrenceCount, 3);
    assert.equal(b.evidencedMaxOutput, 30_000);
    assert.equal(b.evidence.length, 3);
  });

  it('emits one bucket per (source, toolName) pair', async () => {
    const pricing = await loadBuiltinPricing();
    const events = [
      // Claude Bash
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
      // Codex shell
      evt({
        source: 'codex',
        sessionId: 's2',
        toolUseId: 'call_b',
        eventIndex: 0,
        messageId: 'm2',
      }),
      // OpenCode bash
      evt({
        source: 'opencode',
        sessionId: 's3',
        toolUseId: 'opc_c',
        eventIndex: 0,
        messageId: 'm3',
      }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_a', byteLen: 80_000, approxTokens: 20_000 }],
      }),
      userTurn({
        source: 'codex',
        sessionId: 's2',
        userUuid: 'u2',
        precedingMessageId: 'm2',
        followingMessageId: 'm3',
        blocks: [{ kind: 'tool_result', toolUseId: 'call_b', byteLen: 90_000, approxTokens: 22_500 }],
      }),
      userTurn({
        source: 'opencode',
        sessionId: 's3',
        userUuid: 'u3',
        precedingMessageId: 'm3',
        followingMessageId: 'm4',
        blocks: [{ kind: 'tool_result', toolUseId: 'opc_c', byteLen: 85_000, approxTokens: 21_250 }],
      }),
    ];
    const turns: TurnRecord[] = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
      turn({
        source: 'codex',
        sessionId: 's2',
        messageId: 'm2',
        turnIndex: 0,
        toolCalls: [tc('call_b', 'shell')],
      }),
      turn({
        source: 'opencode',
        sessionId: 's3',
        messageId: 'm3',
        turnIndex: 0,
        toolCalls: [tc('opc_c', 'bash')],
      }),
    ];
    const out = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing });
    // Three distinct (source, toolName) buckets — Claude Bash, Codex shell
    // (normalizes to Bash), OpenCode bash (normalizes to Bash) all get their
    // own row, keyed by source. Cross-harness aggregation under the
    // canonical `Bash` name is intentional (#168 acceptance).
    assert.equal(out.length, 3);
    const sources = out.map((b) => b.source).sort();
    assert.deepEqual(sources, ['claude-code', 'codex', 'opencode']);
    for (const b of out) assert.equal(b.toolName, 'Bash');
  });

  it('skips events without matching user-turn blocks', async () => {
    const pricing = await loadBuiltinPricing();
    const events = [
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
    ];
    const userTurns: UserTurnRecord[] = [];
    const turns = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
    ];
    const out = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing });
    assert.equal(out.length, 0);
  });

  it('honors a custom threshold', async () => {
    const pricing = await loadBuiltinPricing();
    // 1,000 tokens — under default 15k but over a 500-token threshold.
    const events = [
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_a', byteLen: 4_000, approxTokens: 1_000 }],
      }),
    ];
    const turns = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
    ];
    const def = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing });
    assert.equal(def.length, 0);
    const tight = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing, threshold: 500 });
    assert.equal(tight.length, 1);
    assert.equal(tight[0]!.evidencedMaxOutput, 1000);
  });

  it('falls back to <unknown> when the tool_use_id has no matching turn', async () => {
    const pricing = await loadBuiltinPricing();
    const events = [
      evt({ sessionId: 's1', toolUseId: 'orphan', eventIndex: 0, messageId: 'm1' }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'orphan', byteLen: 80_000, approxTokens: 20_000 }],
      }),
    ];
    // No turns supplied — the tool name lookup misses.
    const out = detectObservedBloat({ toolResultEvents: events, userTurns, turns: [], pricing });
    assert.equal(out.length, 1);
    assert.equal(out[0]!.toolName, '<unknown>');
    // Without a model we still emit the bucket but cost is 0.
    assert.equal(out[0]!.cost, 0);
  });
});

// ---------------------------------------------------------------------------
// Top-level orchestration
// ---------------------------------------------------------------------------

describe('detectToolOutputBloat — orchestration', () => {
  it('runs both signals when given inputs for both', async () => {
    const pricing = await loadBuiltinPricing();
    const settings: LoadedClaudeSettings[] = [
      { path: '/u/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '80000' } } },
    ];
    const events = [
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_a', byteLen: 80_000, approxTokens: 20_000 }],
      }),
    ];
    const turns = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
    ];
    const out = detectToolOutputBloat({
      settings,
      toolResultEvents: events,
      userTurns,
      turns,
      pricing,
    });
    assert.equal(out.length, 2);
    const kinds = out.map((b) => b.kind).sort();
    assert.deepEqual(kinds, ['observed-bloat', 'static-config']);
  });

  it('runs only Signal A when no events are supplied', async () => {
    const pricing = await loadBuiltinPricing();
    const settings: LoadedClaudeSettings[] = [
      { path: '/u/.claude/settings.json', settings: { env: { [BASH_MAX_OUTPUT_ENV_KEY]: '80000' } } },
    ];
    const out = detectToolOutputBloat({ settings, pricing });
    assert.equal(out.length, 1);
    assert.equal(out[0]!.kind, 'static-config');
  });

  it('runs only Signal B when no settings are supplied', async () => {
    const pricing = await loadBuiltinPricing();
    const events = [
      evt({ sessionId: 's1', toolUseId: 'tu_a', eventIndex: 0, messageId: 'm1' }),
    ];
    const userTurns = [
      userTurn({
        sessionId: 's1',
        userUuid: 'u1',
        precedingMessageId: 'm1',
        followingMessageId: 'm2',
        blocks: [{ kind: 'tool_result', toolUseId: 'tu_a', byteLen: 80_000, approxTokens: 20_000 }],
      }),
    ];
    const turns = [
      turn({ sessionId: 's1', messageId: 'm1', turnIndex: 0, toolCalls: [tc('tu_a', 'Bash')] }),
    ];
    const out = detectToolOutputBloat({ toolResultEvents: events, userTurns, turns, pricing });
    assert.equal(out.length, 1);
    assert.equal(out[0]!.kind, 'observed-bloat');
  });
});

// ---------------------------------------------------------------------------
// WasteFinding adapter
// ---------------------------------------------------------------------------

describe('toolOutputBloatToFinding — adapter', () => {
  it('emits a paste action targeting settings.json for Signal A', () => {
    const f = toolOutputBloatToFinding({
      source: 'claude-code',
      kind: 'static-config',
      toolName: 'Bash',
      // configuredLimit is in chars (raw env value); evidencedMaxOutput is in
      // tokens (consistent with Signal B).
      configuredLimit: 80000,
      evidencedMaxOutput: 20000,
      occurrenceCount: 1,
      cost: 0,
      evidence: ['/u/.claude/settings.json'],
    });
    assert.equal(f.kind, 'tool-output-bloat');
    assert.equal(f.actions.length, 1);
    const action = f.actions[0]!;
    assert.equal(action.type, 'paste');
    assert.match((action as { label: string }).label, /settings\.json/);
    assert.match((action as { text: string }).text, new RegExp(BASH_MAX_OUTPUT_ENV_KEY));
    // Paste suggestion is in chars — at the 15k-token boundary that's 60000.
    // Asserts the unit-conversion fix (Devin review on PR #180).
    assert.match((action as { text: string }).text, /"60000"/);
    // estimatedSavings.tokensPerSession should reflect tokens, not chars.
    assert.equal(f.estimatedSavings?.tokensPerSession, 20000);
  });

  it('emits an instruction-file paste for Signal B', () => {
    const f = toolOutputBloatToFinding({
      source: 'codex',
      kind: 'observed-bloat',
      toolName: 'shell',
      evidencedMaxOutput: 25000,
      evidencedP95Output: 24000,
      occurrenceCount: 4,
      cost: 0.07,
      evidence: ['s1', 's2'],
    });
    assert.equal(f.kind, 'tool-output-bloat');
    assert.equal(f.severity, 'warn'); // 0.07 > 0.05
    assert.match(f.title, /codex shell/);
    assert.match(f.title, /4×/);
    assert.match(f.detail, /head/);
    assert.match(f.detail, /tail/);
    assert.match(f.detail, /grep/);
    assert.equal(f.actions[0]!.type, 'paste');
  });
});

// ---------------------------------------------------------------------------
// Integration — fixture-driven, cross-harness
// ---------------------------------------------------------------------------

describe('detectStaticConfigBloat — settings.json fixture', () => {
  it('flags an oversized BASH_MAX_OUTPUT_LENGTH from the canonical fixture', async () => {
    const fixture = path.join(
      FIXTURES_ROOT,
      'claude',
      'settings',
      'oversized-bash-output-length.json',
    );
    const loaded = await loadClaudeSettings(fixture);
    assert.ok(loaded);
    const result = detectStaticConfigBloat({ settings: [loaded!] });
    assert.equal(result.length, 1);
    // Fixture sets BASH_MAX_OUTPUT_LENGTH=80000 (chars ≈ 20000 tokens >
    // 15000-token threshold).
    assert.equal(result[0]!.configuredLimit, 80000);
    assert.deepEqual(result[0]!.evidence, [fixture]);
  });
});

describe('detectObservedBloat — cross-harness fixtures (#168 acceptance)', () => {
  it('flags Claude Bash oversized output from a real session fixture', async () => {
    const pricing = await loadBuiltinPricing();
    const fixture = path.join(FIXTURES_ROOT, 'claude', 'oversized-bash-output.jsonl');
    const parsed = await parseClaudeSession(fixture);
    // Old fixture doesn't have enriched user-turn blocks, so we pass empty userTurns
    // and rely on the contentLength fallback. The unit tests above cover the
    // enriched data path.
    const out = detectObservedBloat({
      toolResultEvents: parsed.toolResultEvents,
      userTurns: [],
      turns: parsed.turns,
      pricing,
    });
    assert.equal(out.length, 1, 'expected one bloat bucket');
    assert.equal(out[0]!.source, 'claude-code');
    assert.equal(out[0]!.toolName, 'Bash');
    assert.ok(out[0]!.evidencedMaxOutput >= DEFAULT_BLOAT_TOKEN_THRESHOLD);
  });

  it('flags Codex shell oversized output from a real session fixture', async () => {
    const pricing = await loadBuiltinPricing();
    const fixture = path.join(FIXTURES_ROOT, 'codex', 'oversized-shell-output.jsonl');
    const parsed = await parseCodexSession(fixture);
    // Old fixture doesn't have enriched user-turn blocks, so we pass empty userTurns
    // and rely on the contentLength fallback. The unit tests above cover the
    // enriched data path.
    const out = detectObservedBloat({
      toolResultEvents: parsed.toolResultEvents,
      userTurns: [],
      turns: parsed.turns,
      pricing,
    });
    assert.equal(out.length, 1, 'expected one bloat bucket');
    assert.equal(out[0]!.source, 'codex');
    // Codex `shell` normalizes to `Bash` via TOOL_ALIASES so cross-harness
    // aggregation buckets Claude Bash, Codex shell, and OpenCode bash under
    // one canonical name. The original cell stays available via the source
    // discriminator on the row above.
    assert.equal(out[0]!.toolName, 'Bash');
    assert.ok(out[0]!.evidencedMaxOutput >= DEFAULT_BLOAT_TOKEN_THRESHOLD);
  });

  it('flags OpenCode bash oversized output from synthesized records', async () => {
    // OpenCode fixture format is a multi-file directory tree (see
    // tests/fixtures/opencode/<scenario>/storage/...). Re-creating that for
    // the bloat case yields no extra detector coverage beyond what synthetic
    // ToolResultEventRecords already provide — the detector branches purely
    // on `source` + `approxTokens`. Synthesize the records inline and
    // assert the same cross-harness contract the issue calls out.
    const pricing = await loadBuiltinPricing();
    const events: ToolResultEventRecord[] = [
      evt({
        source: 'opencode',
        sessionId: 'ses_bloat',
        toolUseId: 'opc_bash_1',
        eventIndex: 0,
        messageId: 'msg_bloat',
      }),
    ];
    const userTurns: UserTurnRecord[] = [
      userTurn({
        source: 'opencode',
        sessionId: 'ses_bloat',
        userUuid: 'u_bloat',
        precedingMessageId: 'msg_bloat',
        followingMessageId: 'msg_bloat_next',
        blocks: [{ kind: 'tool_result', toolUseId: 'opc_bash_1', byteLen: 80_000, approxTokens: 20_000 }],
      }),
    ];
    const turns: TurnRecord[] = [
      turn({
        source: 'opencode',
        sessionId: 'ses_bloat',
        messageId: 'msg_bloat',
        turnIndex: 0,
        toolCalls: [tc('opc_bash_1', 'bash')],
      }),
    ];
    const out = detectObservedBloat({ toolResultEvents: events, userTurns, turns, pricing });
    assert.equal(out.length, 1);
    assert.equal(out[0]!.source, 'opencode');
    // OpenCode `bash` (lowercase) normalizes to canonical `Bash`.
    assert.equal(out[0]!.toolName, 'Bash');
  });
});
