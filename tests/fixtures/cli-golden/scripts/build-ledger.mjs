// Build the deterministic CLI-golden fixture ledger.
//
// Writes a synthetic ledger to ${RELAYBURN_HOME} that exercises:
//   - all three readers/sources: claude-code, codex, opencode
//   - multiple sessions per source, multiple turns per session
//   - tool-call shapes the activity classifier + hotspots care about
//     (Read, Edit, Bash, Task) so `compare` produces non-empty buckets
//   - a stamp so workflow-id filtering has something to bind to
//
// All token counts, timestamps, message ids, session ids, and project paths
// are hard-coded so re-running the script always produces a byte-identical
// ledger. The Wave 2 PRs that un-ignore the golden test must avoid drifting
// these values without also refreshing the snapshots.
//
// Usage:
//   RELAYBURN_HOME=tests/fixtures/cli-golden/ledger \
//     node tests/fixtures/cli-golden/scripts/build-ledger.mjs

import { readFile, rm, writeFile } from 'node:fs/promises';
import * as path from 'node:path';

import {
  appendTurns,
  appendUserTurns,
  appendToolResultEvents,
  appendRelationships,
  ledgerHome,
  ledgerPath,
  stamp,
} from '@relayburn/ledger';

// stamp() writes ts: new Date().toISOString() — non-deterministic. We
// stamp first, then post-process the ledger to substitute the stamp's
// drifting `ts` with a fixed timestamp so re-running the script produces
// a byte-identical ledger. Keeps the rest of the pipeline (reader,
// archive, indexes) on the supported public API.
const STAMP_FIXED_TS = '2026-04-23T12:00:00.000Z';

const HOME = ledgerHome();

// Wipe any prior generation so re-runs are reproducible. We only delete
// known-burn files inside HOME to avoid clobbering an unrelated dir if a
// caller pointed RELAYBURN_HOME somewhere wrong.
const FILES = [
  'ledger.jsonl',
  'ledger.idx',
  'ledger.content.idx',
  'cursors.json',
  'hwm.json',
  'config.json',
  'archive.sqlite',
  'archive.sqlite-shm',
  'archive.sqlite-wal',
  'burn.sqlite',
  'burn.sqlite-shm',
  'burn.sqlite-wal',
];
for (const name of FILES) {
  await rm(`${HOME}/${name}`, { force: true });
}
await rm(`${HOME}/content`, { recursive: true, force: true });

console.error(`[fixture] writing to ${HOME}`);

// Two coverage shapes we reuse: full per-turn coverage (used by Claude turns
// so `hotspots` attribution doesn't refuse) and a partial Codex coverage
// (no per-turn cache breakdown, no tool-result events). The shapes mirror
// what the real readers emit today; if the readers' output drifts, refresh
// these to match.
const FULL_COVERAGE = {
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
const CODEX_COVERAGE = {
  hasInputTokens: true,
  hasOutputTokens: true,
  hasReasoningTokens: true,
  hasCacheReadTokens: false,
  hasCacheCreateTokens: false,
  hasToolCalls: true,
  hasToolResultEvents: false,
  hasSessionRelationships: false,
  hasRawContent: false,
};

const CLAUDE_SESSION_A = '11111111-1111-1111-1111-111111111111';
const CLAUDE_SESSION_B = '22222222-2222-2222-2222-222222222222';
const CODEX_SESSION = 'sess_30000000000000000000000000000003';
const OPENCODE_SESSION = 'ses_40000000000000000000000000000004';

/**
 * @param {Partial<import('@relayburn/reader').TurnRecord>} overrides
 */
function turn(overrides) {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    messageId: 'msg-1',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    project: '/tmp/golden-project',
    projectKey: 'golden-project',
    usage: {
      input: 1000,
      output: 200,
      reasoning: 0,
      cacheRead: 5000,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    fidelity: { class: 'full', granularity: 'per-turn', coverage: FULL_COVERAGE },
    ...overrides,
  };
}

// --- Claude session A — coding workflow with edits + reads -----------------
await appendTurns([
  turn({
    sessionId: CLAUDE_SESSION_A,
    messageId: 'msg-c1-1',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    activity: 'coding',
    hasEdits: false,
    toolCalls: [
      { id: 'tu-c1-r1', name: 'Read', target: '/tmp/golden-project/src/foo.ts', argsHash: 'r1' },
    ],
    usage: {
      input: 1500, output: 220, reasoning: 0,
      cacheRead: 5000, cacheCreate5m: 0, cacheCreate1h: 0,
    },
  }),
  turn({
    sessionId: CLAUDE_SESSION_A,
    messageId: 'msg-c1-2',
    turnIndex: 1,
    ts: '2026-04-20T00:01:00.000Z',
    model: 'claude-sonnet-4-6',
    activity: 'coding',
    hasEdits: true,
    toolCalls: [
      {
        id: 'tu-c1-e1',
        name: 'Edit',
        target: '/tmp/golden-project/src/foo.ts',
        argsHash: 'e1',
        editPreHash: 'pre1',
        editPostHash: 'post1',
      },
    ],
    usage: {
      input: 1800, output: 350, reasoning: 0,
      cacheRead: 6000, cacheCreate5m: 200, cacheCreate1h: 0,
    },
  }),
  turn({
    sessionId: CLAUDE_SESSION_A,
    messageId: 'msg-c1-3',
    turnIndex: 2,
    ts: '2026-04-20T00:02:00.000Z',
    model: 'claude-sonnet-4-6',
    activity: 'testing',
    hasEdits: false,
    toolCalls: [
      { id: 'tu-c1-b1', name: 'Bash', target: 'npm test', argsHash: 'b1' },
    ],
    usage: {
      input: 1200, output: 180, reasoning: 0,
      cacheRead: 7000, cacheCreate5m: 0, cacheCreate1h: 0,
    },
  }),
]);

// --- Claude session B — same model A + a haiku turn (compare needs ≥2 models)
await appendTurns([
  turn({
    sessionId: CLAUDE_SESSION_B,
    messageId: 'msg-c2-1',
    turnIndex: 0,
    ts: '2026-04-21T00:00:00.000Z',
    model: 'claude-haiku-4-5',
    activity: 'coding',
    hasEdits: true,
    toolCalls: [
      {
        id: 'tu-c2-e1',
        name: 'Edit',
        target: '/tmp/golden-project/src/bar.ts',
        argsHash: 'e2',
        editPreHash: 'pre2',
        editPostHash: 'post2',
      },
    ],
    usage: {
      input: 900, output: 120, reasoning: 0,
      cacheRead: 2000, cacheCreate5m: 0, cacheCreate1h: 0,
    },
  }),
  turn({
    sessionId: CLAUDE_SESSION_B,
    messageId: 'msg-c2-2',
    turnIndex: 1,
    ts: '2026-04-21T00:01:00.000Z',
    model: 'claude-haiku-4-5',
    activity: 'review',
    hasEdits: false,
    toolCalls: [
      { id: 'tu-c2-r1', name: 'Read', target: '/tmp/golden-project/src/bar.ts', argsHash: 'r2' },
    ],
    usage: {
      input: 800, output: 100, reasoning: 0,
      cacheRead: 2500, cacheCreate5m: 0, cacheCreate1h: 0,
    },
  }),
]);

// --- Codex session — codex source + reasoning tokens, partial coverage ----
await appendTurns([
  {
    v: 1,
    source: 'codex',
    sessionId: CODEX_SESSION,
    messageId: 'msg-cdx-1',
    turnIndex: 0,
    ts: '2026-04-22T00:00:00.000Z',
    model: 'gpt-5-codex',
    project: '/tmp/golden-project',
    projectKey: 'golden-project',
    activity: 'coding',
    hasEdits: false,
    toolCalls: [
      { id: 'tu-cdx-1', name: 'shell', target: 'cargo build', argsHash: 'sh1' },
    ],
    usage: {
      input: 2000, output: 400, reasoning: 350,
      cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0,
    },
    fidelity: { class: 'partial', granularity: 'per-turn', coverage: CODEX_COVERAGE },
  },
]);

// --- OpenCode session — opencode source + a Task spawn (subagent stub) ----
await appendTurns([
  {
    v: 1,
    source: 'opencode',
    sessionId: OPENCODE_SESSION,
    messageId: 'msg-opn-1',
    turnIndex: 0,
    ts: '2026-04-23T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    project: '/tmp/golden-project',
    projectKey: 'golden-project',
    activity: 'delegation',
    hasEdits: false,
    toolCalls: [
      { id: 'tu-opn-task', name: 'Task', target: 'review the foo module', argsHash: 'tk1' },
    ],
    usage: {
      input: 600, output: 80, reasoning: 0,
      cacheRead: 1500, cacheCreate5m: 0, cacheCreate1h: 0,
    },
    fidelity: { class: 'full', granularity: 'per-turn', coverage: FULL_COVERAGE },
  },
]);

// --- Stamp — workflow attribution for `--workflow` filtering --------------
await stamp({ sessionId: CLAUDE_SESSION_A }, { workflowId: 'wf-golden' });

// --- User turns — let hotspots attribute Read/Edit on session A by size ---
await appendUserTurns([
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    userUuid: 'u-c1-pre-msg-1',
    ts: '2026-04-20T00:00:00.000Z',
    followingMessageId: 'msg-c1-1',
    blocks: [{ kind: 'text', byteLen: 32, approxTokens: 8 }],
  },
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    userUuid: 'u-c1-pre-msg-2',
    ts: '2026-04-20T00:00:30.000Z',
    precedingMessageId: 'msg-c1-1',
    followingMessageId: 'msg-c1-2',
    blocks: [
      { kind: 'tool_result', toolUseId: 'tu-c1-r1', byteLen: 4000, approxTokens: 1000 },
    ],
  },
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    userUuid: 'u-c1-pre-msg-3',
    ts: '2026-04-20T00:01:30.000Z',
    precedingMessageId: 'msg-c1-2',
    followingMessageId: 'msg-c1-3',
    blocks: [
      { kind: 'tool_result', toolUseId: 'tu-c1-e1', byteLen: 800, approxTokens: 200 },
    ],
  },
]);

// --- Tool-result events — keeps hotspots attribution out of refusal mode --
await appendToolResultEvents([
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    toolUseId: 'tu-c1-r1',
    ts: '2026-04-20T00:00:30.000Z',
    eventSource: 'transcript',
    status: 'completed',
    contentLength: 4000,
  },
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    toolUseId: 'tu-c1-e1',
    ts: '2026-04-20T00:01:30.000Z',
    eventSource: 'transcript',
    status: 'completed',
    contentLength: 800,
  },
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    toolUseId: 'tu-c1-b1',
    ts: '2026-04-20T00:02:30.000Z',
    eventSource: 'transcript',
    status: 'completed',
    contentLength: 1200,
  },
]);

// --- Relationships — rooted Claude sessions + a subagent edge from opencode
await appendRelationships([
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_A,
    relationshipType: 'root',
    ts: '2026-04-20T00:00:00.000Z',
  },
  {
    v: 1,
    source: 'claude-code',
    sessionId: CLAUDE_SESSION_B,
    relationshipType: 'root',
    ts: '2026-04-21T00:00:00.000Z',
  },
  {
    v: 1,
    source: 'codex',
    sessionId: CODEX_SESSION,
    relationshipType: 'root',
    ts: '2026-04-22T00:00:00.000Z',
  },
  {
    v: 1,
    source: 'opencode',
    sessionId: OPENCODE_SESSION,
    relationshipType: 'root',
    ts: '2026-04-23T00:00:00.000Z',
  },
]);

// Substitute the stamp's wall-clock ts for the fixed value so the ledger
// hashes the same on every run. Other ledger lines have hand-pinned ts
// values already; only stamp() inserts a live timestamp.
const ledgerFile = ledgerPath();
const raw = await readFile(ledgerFile, 'utf8');
const rewritten = raw.replace(
  /("kind":"stamp","ts":")[^"]+(")/g,
  `$1${STAMP_FIXED_TS}$2`,
);
if (rewritten !== raw) {
  await writeFile(ledgerFile, rewritten);
}

console.error('[fixture] done');
